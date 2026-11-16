//! The sync client: one-shot [`SyncClient::sync_once`] and the long-running [`SyncClient::run`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::base::{ContentHash, Error, Result, SourceId, UnitKey, UnitMeta};
use crate::config::Depth;
use crate::engine::depth::unit_in_depth;

use super::config::{SyncConfig, SyncTransport};
use super::identity::{DeviceIdentity, TrustPolicy};
use super::pack::PackLimits;
use super::pb;
use super::pb::anchor_sync_client::AnchorSyncClient;
use super::protocol::{
    BATCH_UNITS, CHUNK_BYTES, Hello, Local, Mode, PROTOCOL_VERSION, PushResult, Request, Response,
    ServerHello, missing_keys,
};
use super::store::{Rejection, UnitStore};

const MAX_MESSAGE_BYTES: usize = 64 << 20;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// What one synchronisation round did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SyncReport {
    /// Unit versions the server applied from us.
    pub pushed: u64,
    /// Unit versions we applied from the server.
    pub pulled: u64,
    /// Unit versions either side refused.
    pub rejected: u64,
    /// Pack bytes sent and received.
    pub bytes: u64,
    /// Wall-clock time of the round.
    #[serde(serialize_with = "serialize_duration")]
    pub duration: Duration,
}

fn serialize_duration<S: serde::Serializer>(
    d: &Duration,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_f64(d.as_secs_f64())
}

/// Client configuration.
#[derive(Clone)]
pub struct ClientOptions {
    /// Server URL (`ws://host:port[/sync]`, `http://host:port` for gRPC).
    pub url: String,
    /// Transport to use.
    pub transport: SyncTransport,
    /// Bearer token.
    pub token: Option<String>,
    /// Signs outgoing packs.
    pub identity: Arc<DeviceIdentity>,
    /// Accepted signers of packs from the server.
    pub trust: TrustPolicy,
    /// Bounds for packs from the server.
    pub limits: PackLimits,
    /// Sources to publish (empty: nothing).
    pub publish: Vec<SourceId>,
    /// Sources to pull (empty: everything).
    pub subscribe: Vec<SourceId>,
    /// Depth each subscribed source is pulled at (absent: full). Below full, only the units
    /// that depth materialises are requested.
    pub depths: HashMap<SourceId, Depth>,
    /// Depth of subscribed sources missing from `depths`.
    pub default_depth: Depth,
    /// Seconds between rounds without a notification.
    pub interval: Duration,
    /// Connection establishment timeout.
    pub connect_timeout: Duration,
    /// Longest wait for the next message of an exchange.
    pub io_timeout: Duration,
    /// Largest accepted pack.
    pub max_pack_bytes: usize,
}

impl ClientOptions {
    /// Options from `[sync]`; fails when no server is configured.
    pub fn from_config(
        config: &SyncConfig,
        identity: Arc<DeviceIdentity>,
        token: Option<String>,
        trust: TrustPolicy,
    ) -> Result<Self> {
        let url = config
            .server_url
            .clone()
            .ok_or_else(|| Error::config("sync.server_url is not set"))?;
        Ok(Self {
            url,
            transport: config.transport,
            token,
            identity,
            trust,
            limits: PackLimits::default(),
            publish: config.publish.clone(),
            subscribe: config.subscribe.clone(),
            depths: HashMap::new(),
            default_depth: Depth::Full,
            interval: Duration::from_secs(config.interval_secs.max(1)),
            connect_timeout: Duration::from_secs(10),
            io_timeout: Duration::from_secs(30),
            max_pack_bytes: 256 << 20,
        })
    }
}

/// A sync client over a [`UnitStore`].
#[derive(Clone)]
pub struct SyncClient {
    local: Local,
    opts: Arc<ClientOptions>,
}

impl SyncClient {
    /// A client syncing `store` with the server in `options`.
    pub fn new(store: Arc<dyn UnitStore>, options: ClientOptions) -> Self {
        Self {
            local: Local {
                store,
                identity: options.identity.clone(),
                trust: options.trust.clone(),
                limits: options.limits,
            },
            opts: Arc::new(options),
        }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Local) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let local = self.local.clone();
        tokio::task::spawn_blocking(move || f(&local)).await?
    }

    fn hello(&self, mode: Mode, roots: BTreeMap<SourceId, ContentHash>) -> Hello {
        Hello {
            version: PROTOCOL_VERSION,
            device_id: self.opts.identity.device_id().as_str().to_owned(),
            token: self.opts.token.clone(),
            mode,
            roots,
        }
    }

    async fn connect(&self) -> Result<Box<dyn Peer>> {
        let o = &self.opts;
        let connecting = async {
            match o.transport {
                SyncTransport::WebSocket => WsPeer::connect(o)
                    .await
                    .map(|p| Box::new(p) as Box<dyn Peer>),
                SyncTransport::Grpc => GrpcPeer::connect(o)
                    .await
                    .map(|p| Box::new(p) as Box<dyn Peer>),
            }
        };
        timeout(o.connect_timeout, connecting)
            .await
            .map_err(|_| Error::sync("connecting timed out"))?
    }

    /// One synchronisation round: push what the server lacks, pull what we lack.
    pub async fn sync_once(&self) -> Result<SyncReport> {
        let started = Instant::now();
        let mut peer = self.connect().await?;
        let local_roots = self.blocking(|l| l.roots()).await?;
        let server = peer
            .hello(self.hello(Mode::Sync, local_roots.clone()))
            .await?;
        if server.version != PROTOCOL_VERSION {
            return Err(Error::sync("the server speaks another protocol version"));
        }
        let differs = |ns: &SourceId| {
            local_roots.get(ns).copied().unwrap_or(ContentHash::ZERO)
                != server.roots.get(ns).copied().unwrap_or(ContentHash::ZERO)
        };
        let push_ns: Vec<SourceId> = self
            .opts
            .publish
            .iter()
            .filter(|ns| local_roots.contains_key(*ns) && differs(ns))
            .cloned()
            .collect();
        let subscribe: BTreeSet<&SourceId> = self.opts.subscribe.iter().collect();
        let pull_ns: Vec<SourceId> = server
            .roots
            .keys()
            .filter(|ns| (subscribe.is_empty() || subscribe.contains(ns)) && differs(ns))
            .cloned()
            .collect();
        let wanted: Vec<SourceId> = push_ns
            .iter()
            .chain(&pull_ns)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .cloned()
            .collect();
        let mut report = SyncReport::default();
        if wanted.is_empty() {
            report.duration = started.elapsed();
            return Ok(report);
        }
        let remote_metas = peer.metas(&wanted).await?;
        let own = wanted.clone();
        let local_metas = self.blocking(move |l| l.metas(&own)).await?;
        let empty: Vec<UnitMeta> = Vec::new();
        let collect = |namespaces: &[SourceId],
                       from: &BTreeMap<SourceId, Vec<UnitMeta>>,
                       to: &BTreeMap<SourceId, Vec<UnitMeta>>| {
            namespaces
                .iter()
                .flat_map(|ns| {
                    missing_keys(from.get(ns).unwrap_or(&empty), to.get(ns).unwrap_or(&empty))
                })
                .collect::<Vec<UnitKey>>()
        };
        let to_push = collect(&push_ns, &local_metas, &remote_metas);
        // Below full depth, units the depth does not materialise are not requested; they are
        // pulled when the depth for the source rises.
        let in_depth: BTreeMap<SourceId, Vec<UnitMeta>> = remote_metas
            .iter()
            .map(|(ns, metas)| {
                let depth = self
                    .opts
                    .depths
                    .get(ns)
                    .copied()
                    .unwrap_or(self.opts.default_depth);
                let kept = metas
                    .iter()
                    .filter(|meta| unit_in_depth(meta.kind, depth))
                    .cloned()
                    .collect();
                (ns.clone(), kept)
            })
            .collect();
        let to_pull = collect(&pull_ns, &in_depth, &local_metas);

        for keys in to_push.chunks(BATCH_UNITS) {
            let (keys, base) = (keys.to_vec(), server.roots.clone());
            let pack = self.blocking(move |l| l.build_pack(&keys, &base)).await?;
            report.bytes += pack.len() as u64;
            let PushResult {
                applied, rejected, ..
            } = peer.push(pack).await?;
            report.pushed += applied;
            report.rejected += rejected.len() as u64;
        }
        for keys in to_pull.chunks(BATCH_UNITS) {
            let pack = peer.pull(keys.to_vec(), local_roots.clone()).await?;
            report.bytes += pack.len() as u64;
            let applied = self.blocking(move |l| l.apply_pack(&pack)).await?;
            report.pulled += applied.applied.len() as u64;
            report.rejected += applied.rejected.len() as u64;
        }
        report.duration = started.elapsed();
        Ok(report)
    }

    /// Syncs now, then again on every change notification and every `interval`, reconnecting
    /// with capped exponential backoff after failures, until `shutdown` completes.
    pub async fn run(&self, shutdown: impl Future<Output = ()>) {
        let cancel = CancellationToken::new();
        let notify = Arc::new(Notify::new());
        let watcher = tokio::spawn(self.clone().watch_loop(notify.clone(), cancel.clone()));
        tokio::pin!(shutdown);
        let mut backoff = Duration::from_secs(1);
        loop {
            let wait = match self.sync_once().await {
                Ok(report) => {
                    if report.pushed + report.pulled + report.rejected > 0 {
                        info!(?report, "sync round finished");
                    }
                    backoff = Duration::from_secs(1);
                    self.opts.interval
                }
                Err(err) => {
                    warn!("sync failed: {err}; retrying in {backoff:?}");
                    let wait = backoff;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    wait
                }
            };
            tokio::select! {
                () = &mut shutdown => break,
                () = notify.notified() => {}
                () = sleep(wait) => {}
            }
        }
        cancel.cancel();
        let _ = watcher.await;
    }

    async fn watch_loop(self, notify: Arc<Notify>, cancel: CancellationToken) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let watching = async {
                let mut peer = self.connect().await?;
                peer.hello(self.hello(Mode::Watch, BTreeMap::new())).await?;
                // Changes may have been missed while disconnected.
                notify.notify_one();
                loop {
                    peer.next_change().await?;
                    backoff = Duration::from_secs(1);
                    notify.notify_one();
                }
                #[allow(unreachable_code)]
                Ok::<(), Error>(())
            };
            tokio::select! {
                () = cancel.cancelled() => return,
                result = watching => {
                    if let Err(err) = result {
                        warn!("change watch lost: {err}");
                    }
                }
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }
}

// ---- transports ---------------------------------------------------------------------------

/// One connection to a server; the same exchange over either transport.
#[async_trait]
pub(crate) trait Peer: Send {
    async fn hello(&mut self, hello: Hello) -> Result<ServerHello>;
    async fn metas(&mut self, sources: &[SourceId]) -> Result<BTreeMap<SourceId, Vec<UnitMeta>>>;
    async fn push(&mut self, pack: Vec<u8>) -> Result<PushResult>;
    async fn pull(
        &mut self,
        keys: Vec<UnitKey>,
        base_roots: BTreeMap<SourceId, ContentHash>,
    ) -> Result<Vec<u8>>;
    /// Waits for the next change notification (watch connections).
    async fn next_change(&mut self) -> Result<()>;
}

/// Splits `scheme://rest` and returns the rest.
fn authority_and_path(url: &str) -> (&str, &str) {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    }
}

fn is_tls(url: &str) -> bool {
    ["wss://", "https://", "grpcs://"]
        .iter()
        .any(|s| url.starts_with(s))
}

pub(crate) struct WsPeer {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    io: Duration,
    max_pack: usize,
}

impl WsPeer {
    pub(crate) async fn connect(o: &ClientOptions) -> Result<Self> {
        let (authority, path) = authority_and_path(&o.url);
        let url = format!(
            "{}://{authority}{}",
            if is_tls(&o.url) { "wss" } else { "ws" },
            if path.is_empty() || path == "/" {
                "/sync"
            } else {
                path
            }
        );
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let (ws, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), true)
            .await
            .map_err(|err| Error::sync(format!("websocket connect failed: {err}")))?;
        Ok(Self {
            ws,
            io: o.io_timeout,
            max_pack: o.max_pack_bytes,
        })
    }

    async fn send(&mut self, message: Message) -> Result<()> {
        timeout(self.io, self.ws.send(message))
            .await
            .map_err(|_| Error::sync("send timed out"))?
            .map_err(|err| Error::sync(format!("send failed: {err}")))
    }

    async fn send_request(&mut self, request: &Request) -> Result<()> {
        self.send(Message::Text(serde_json::to_string(request)?.into()))
            .await
    }

    async fn recv(&mut self, wait: Duration) -> Result<Message> {
        loop {
            match timeout(wait, self.ws.next()).await {
                Err(_) => return Err(Error::sync("timed out waiting for the server")),
                Ok(None | Some(Ok(Message::Close(_)))) => {
                    return Err(Error::sync("the server closed the connection"));
                }
                Ok(Some(Err(err))) => return Err(Error::sync(format!("receive failed: {err}"))),
                Ok(Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)))) => {}
                Ok(Some(Ok(message))) => return Ok(message),
            }
        }
    }

    async fn recv_response(&mut self, wait: Duration) -> Result<Response> {
        match self.recv(wait).await? {
            Message::Text(text) => match serde_json::from_str(text.as_str())? {
                Response::Error { message } => Err(Error::sync(format!("server: {message}"))),
                response => Ok(response),
            },
            _ => Err(Error::sync("expected a control message")),
        }
    }
}

#[async_trait]
impl Peer for WsPeer {
    async fn hello(&mut self, hello: Hello) -> Result<ServerHello> {
        self.send_request(&Request::Hello(hello)).await?;
        match self.recv_response(self.io).await? {
            Response::Hello(hello) => Ok(hello),
            _ => Err(Error::sync("unexpected answer to hello")),
        }
    }

    async fn metas(&mut self, sources: &[SourceId]) -> Result<BTreeMap<SourceId, Vec<UnitMeta>>> {
        self.send_request(&Request::Metas {
            sources: sources.to_vec(),
        })
        .await?;
        match self.recv_response(self.io).await? {
            Response::Metas { metas } => Ok(metas),
            _ => Err(Error::sync("unexpected answer to metas")),
        }
    }

    async fn push(&mut self, pack: Vec<u8>) -> Result<PushResult> {
        self.send_request(&Request::Push {
            size: pack.len() as u64,
        })
        .await?;
        for chunk in pack.chunks(CHUNK_BYTES) {
            self.send(Message::Binary(chunk.to_vec().into())).await?;
        }
        match self.recv_response(self.io).await? {
            Response::Pushed(result) => Ok(result),
            _ => Err(Error::sync("unexpected answer to push")),
        }
    }

    async fn pull(
        &mut self,
        keys: Vec<UnitKey>,
        base_roots: BTreeMap<SourceId, ContentHash>,
    ) -> Result<Vec<u8>> {
        self.send_request(&Request::Pull { keys, base_roots })
            .await?;
        let Response::Pack { size } = self.recv_response(self.io).await? else {
            return Err(Error::sync("unexpected answer to pull"));
        };
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        if size > self.max_pack {
            return Err(Error::sync("the server's pack exceeds the size limit"));
        }
        let mut pack = Vec::with_capacity(size.min(CHUNK_BYTES));
        while pack.len() < size {
            match self.recv(self.io).await? {
                Message::Binary(data) if pack.len() + data.len() <= size => {
                    pack.extend_from_slice(&data);
                }
                _ => return Err(Error::sync("malformed pack transfer")),
            }
        }
        Ok(pack)
    }

    async fn next_change(&mut self) -> Result<()> {
        // The server pings every 30 s; silence for three times that means a dead connection.
        match self.recv_response(Duration::from_secs(90)).await? {
            Response::Changed { .. } => Ok(()),
            _ => Err(Error::sync("unexpected message on the watch channel")),
        }
    }
}

struct GrpcPeer {
    client: AnchorSyncClient<tonic::transport::Channel>,
    token: Option<String>,
    device: String,
    io: Duration,
    max_pack: usize,
    watch: Option<tonic::Streaming<pb::Change>>,
}

fn grpc_err(status: tonic::Status) -> Error {
    Error::sync(format!("grpc: {}", status.message()))
}

fn hex_roots(roots: &BTreeMap<SourceId, ContentHash>) -> std::collections::HashMap<String, String> {
    roots
        .iter()
        .map(|(s, r)| (s.as_str().to_owned(), r.to_hex()))
        .collect()
}

impl GrpcPeer {
    async fn connect(o: &ClientOptions) -> Result<Self> {
        let (authority, _) = authority_and_path(&o.url);
        let tls = is_tls(&o.url);
        let url = format!("{}://{authority}", if tls { "https" } else { "http" });
        let mut endpoint = tonic::transport::Endpoint::from_shared(url)
            .map_err(|err| Error::config(format!("invalid server url: {err}")))?
            .connect_timeout(o.connect_timeout)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(20))
            .keep_alive_while_idle(true);
        if tls {
            endpoint = endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new().with_webpki_roots())
                .map_err(|err| Error::config(format!("tls configuration failed: {err}")))?;
        }
        let channel = endpoint
            .connect()
            .await
            .map_err(|err| Error::sync(format!("grpc connect failed: {err}")))?;
        Ok(Self {
            client: AnchorSyncClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
            token: o.token.clone(),
            device: o.identity.device_id().as_str().to_owned(),
            io: o.io_timeout,
            max_pack: o.max_pack_bytes,
            watch: None,
        })
    }

    fn req<T>(&self, message: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        if let Some(token) = &self.token
            && let Ok(value) = format!("Bearer {token}").parse()
        {
            request.metadata_mut().insert("authorization", value);
        }
        if let Ok(value) = self.device.parse() {
            request.metadata_mut().insert("x-anchor-device", value);
        }
        request
    }
}

#[async_trait]
impl Peer for GrpcPeer {
    async fn hello(&mut self, hello: Hello) -> Result<ServerHello> {
        let request = self.req(pb::HelloRequest {
            protocol_version: hello.version,
            device_id: hello.device_id,
            roots: hex_roots(&hello.roots),
        });
        let reply = timeout(self.io, self.client.hello(request))
            .await
            .map_err(|_| Error::sync("hello timed out"))?
            .map_err(grpc_err)?
            .into_inner();
        let roots = reply
            .roots
            .iter()
            .map(|(s, h)| Ok((SourceId::new(s.as_str()), ContentHash::from_hex(h)?)))
            .collect::<Result<_>>()?;
        Ok(ServerHello {
            version: reply.protocol_version,
            device_id: reply.device_id,
            roots,
        })
    }

    async fn metas(&mut self, sources: &[SourceId]) -> Result<BTreeMap<SourceId, Vec<UnitMeta>>> {
        let request = self.req(pb::MetasRequest {
            sources: sources.iter().map(|s| s.as_str().to_owned()).collect(),
        });
        let reply = timeout(self.io, self.client.metas(request))
            .await
            .map_err(|_| Error::sync("metas timed out"))?
            .map_err(grpc_err)?
            .into_inner();
        Ok(serde_json::from_slice(&reply.json)?)
    }

    async fn push(&mut self, pack: Vec<u8>) -> Result<PushResult> {
        let chunks: Vec<pb::PackChunk> = pack
            .chunks(CHUNK_BYTES)
            .map(|c| pb::PackChunk { data: c.to_vec() })
            .collect();
        let request = self.req(tokio_stream::iter(chunks));
        let reply = timeout(self.io * 4, self.client.push(request))
            .await
            .map_err(|_| Error::sync("push timed out"))?
            .map_err(grpc_err)?
            .into_inner();
        Ok(PushResult {
            applied: reply.applied,
            stale: reply.stale,
            rejected: reply
                .rejected
                .into_iter()
                .map(|reason| Rejection {
                    key: UnitKey::new(""),
                    reason,
                })
                .collect(),
        })
    }

    async fn pull(
        &mut self,
        keys: Vec<UnitKey>,
        base_roots: BTreeMap<SourceId, ContentHash>,
    ) -> Result<Vec<u8>> {
        let request = self.req(pb::PullRequest {
            keys: keys.iter().map(|k| k.as_str().to_owned()).collect(),
            base_roots: hex_roots(&base_roots),
        });
        let mut stream = timeout(self.io, self.client.pull(request))
            .await
            .map_err(|_| Error::sync("pull timed out"))?
            .map_err(grpc_err)?
            .into_inner();
        let mut pack = Vec::new();
        while let Some(chunk) = timeout(self.io, stream.message())
            .await
            .map_err(|_| Error::sync("pull timed out"))?
            .map_err(grpc_err)?
        {
            if pack.len() + chunk.data.len() > self.max_pack {
                return Err(Error::sync("the server's pack exceeds the size limit"));
            }
            pack.extend_from_slice(&chunk.data);
        }
        Ok(pack)
    }

    async fn next_change(&mut self) -> Result<()> {
        if self.watch.is_none() {
            let request = self.req(pb::WatchRequest {
                device_id: self.device.clone(),
            });
            let stream = timeout(self.io, self.client.watch(request))
                .await
                .map_err(|_| Error::sync("watch timed out"))?
                .map_err(grpc_err)?
                .into_inner();
            self.watch = Some(stream);
        }
        let Some(stream) = self.watch.as_mut() else {
            return Err(Error::sync("watch stream missing"));
        };
        match stream.message().await.map_err(grpc_err)? {
            Some(_) => Ok(()),
            None => Err(Error::sync("the server ended the watch stream")),
        }
    }
}

crate::base::opaque_debug!(ClientOptions, SyncClient);
