//! The sync server: WebSocket route `/sync` and the gRPC service on one listener.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{Stream, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response as GrpcResponse, Status, Streaming};
use tracing::{debug, warn};

use crate::base::{ContentHash, Error, Result, SourceId, UnitKey};

use super::identity::{DeviceIdentity, TrustPolicy};
use super::pack::PackLimits;
use super::pb;
use super::pb::anchor_sync_server::{AnchorSync, AnchorSyncServer};
use super::protocol::{
    CHUNK_BYTES, Hello, Local, Mode, PROTOCOL_VERSION, PushResult, Request as Req,
    Response as Resp, ServerHello,
};
use super::store::UnitStore;

/// Most keys a single pull may ask for.
const MAX_PULL_KEYS: usize = 20_000;
/// Largest WebSocket message (control messages carry meta lists).
const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// A namespace change announced to watching clients.
#[derive(Clone, Debug)]
pub struct Change {
    /// Namespace that changed.
    pub source: SourceId,
    /// Device that caused it.
    pub origin: String,
}

/// Server configuration.
#[derive(Clone)]
pub struct ServerOptions {
    /// Signs the packs the server sends; its id is the server's device id.
    pub identity: Arc<DeviceIdentity>,
    /// Required bearer token. Without one, only loopback peers are accepted.
    pub token: Option<String>,
    /// Accepted signers of pushed packs.
    pub trust: TrustPolicy,
    /// Bounds for pushed packs.
    pub limits: PackLimits,
    /// Largest accepted pack on the wire.
    pub max_pack_bytes: usize,
    /// Longest wait for the next message of a started exchange.
    pub io_timeout: Duration,
    /// Longest a sync connection may sit idle between requests.
    pub idle_timeout: Duration,
    /// Simultaneous connections.
    pub max_connections: usize,
    /// Change feed; send on it to announce changes made outside the sync path.
    pub changes: broadcast::Sender<Change>,
}

impl ServerOptions {
    /// Defaults: trust any valid signer, 256 MiB packs, 30 s I/O timeout, 64 connections.
    pub fn new(identity: Arc<DeviceIdentity>, token: Option<String>) -> Self {
        Self {
            identity,
            token,
            trust: TrustPolicy::AnyValid,
            limits: PackLimits::default(),
            max_pack_bytes: 256 << 20,
            io_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(300),
            max_connections: 64,
            changes: broadcast::channel(256).0,
        }
    }
}

/// Reads the token named by `var` through `lookup` (injectable for tests; the CLI passes
/// `|name| std::env::var(name).ok()`). Empty values count as absent.
pub fn token_from_env(
    var: Option<&str>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    var.and_then(lookup).filter(|token| !token.is_empty())
}

/// Constant-time comparison of two tokens (both are hashed first, so length does not leak).
pub fn tokens_match(a: &str, b: &str) -> bool {
    let (a, b) = (blake3::hash(a.as_bytes()), blake3::hash(b.as_bytes()));
    a.as_bytes()
        .iter()
        .zip(b.as_bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

struct Core {
    local: Local,
    opts: ServerOptions,
    permits: Arc<Semaphore>,
    cancel: CancellationToken,
}

impl Core {
    fn authorize(&self, peer: Option<IpAddr>, presented: Option<&str>) -> bool {
        match &self.opts.token {
            Some(expected) => presented.is_some_and(|got| tokens_match(expected, got)),
            None => peer.is_some_and(|ip| ip.is_loopback()),
        }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Local) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let local = self.local.clone();
        tokio::task::spawn_blocking(move || f(&local)).await?
    }

    /// Verifies and merges a pushed pack, then announces the changed namespaces.
    async fn accept_pack(&self, pack: Vec<u8>, origin: &str) -> Result<PushResult> {
        let report = self.blocking(move |local| local.apply_pack(&pack)).await?;
        let mut sources: Vec<&SourceId> = report.applied.iter().map(|m| &m.source).collect();
        sources.sort();
        sources.dedup();
        for source in sources {
            let _ = self.opts.changes.send(Change {
                source: source.clone(),
                origin: origin.to_owned(),
            });
        }
        Ok(PushResult::from(&report))
    }

    async fn pack_for(
        &self,
        keys: Vec<UnitKey>,
        base_roots: BTreeMap<SourceId, ContentHash>,
    ) -> Result<Vec<u8>> {
        if keys.len() > MAX_PULL_KEYS {
            return Err(Error::invalid("too many keys in one pull"));
        }
        let pack = self
            .blocking(move |local| local.build_pack(&keys, &base_roots))
            .await?;
        if pack.len() > self.opts.max_pack_bytes {
            return Err(Error::invalid(
                "pack exceeds the size limit; pull fewer keys",
            ));
        }
        Ok(pack)
    }
}

/// Entry point of the server.
pub struct SyncServer;

impl SyncServer {
    /// Serves WebSocket (`/sync`) and gRPC on `listener` until `shutdown` completes. Store work
    /// runs on blocking threads.
    pub async fn serve(
        listener: TcpListener,
        store: Arc<dyn UnitStore>,
        options: ServerOptions,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let cancel = CancellationToken::new();
        let core = Arc::new(Core {
            local: Local {
                store,
                identity: options.identity.clone(),
                trust: options.trust.clone(),
                limits: options.limits,
            },
            permits: Arc::new(Semaphore::new(options.max_connections)),
            opts: options,
            cancel: cancel.clone(),
        });
        let grpc = tonic::service::Routes::new(AnchorSyncServer::new(Grpc(core.clone())))
            .into_axum_router();
        let app = Router::new()
            .route("/sync", get(ws_upgrade))
            .with_state(core)
            .merge(grpc);
        let stop = cancel.clone();
        let serving = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            shutdown.await;
            stop.cancel();
        });
        let grace = async {
            cancel.cancelled().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        };
        tokio::select! {
            result = serving => result.map_err(Error::from),
            () = grace => Ok(()),
        }
    }
}

// ---- WebSocket ----------------------------------------------------------------------------

async fn ws_upgrade(
    State(core): State<Arc<Core>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    upgrade: WebSocketUpgrade,
) -> axum::response::Response {
    let Ok(permit) = core.permits.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            if let Err(err) = session(socket, &core, peer.ip()).await {
                debug!("sync session with {peer} ended: {err}");
            }
        })
}

async fn send(ws: &mut WebSocket, core: &Core, message: Message) -> Result<()> {
    timeout(core.opts.io_timeout, ws.send(message))
        .await
        .map_err(|_| Error::sync("send timed out"))?
        .map_err(|err| Error::sync(format!("send failed: {err}")))
}

async fn send_resp(ws: &mut WebSocket, core: &Core, resp: &Resp) -> Result<()> {
    send(ws, core, Message::Text(serde_json::to_string(resp)?.into())).await
}

/// Next data frame within `wait`; `None` on a clean close.
async fn recv(ws: &mut WebSocket, wait: Duration) -> Result<Option<Message>> {
    loop {
        match timeout(wait, ws.recv()).await {
            Err(_) => return Err(Error::sync("timed out waiting for the peer")),
            Ok(None | Some(Ok(Message::Close(_)))) => return Ok(None),
            Ok(Some(Err(err))) => return Err(Error::sync(format!("receive failed: {err}"))),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            Ok(Some(Ok(message))) => return Ok(Some(message)),
        }
    }
}

async fn recv_request(ws: &mut WebSocket, wait: Duration) -> Result<Option<Req>> {
    match recv(ws, wait).await? {
        None => Ok(None),
        Some(Message::Text(text)) => Ok(Some(serde_json::from_str(text.as_str())?)),
        Some(_) => Err(Error::sync("expected a control message")),
    }
}

async fn recv_pack(ws: &mut WebSocket, core: &Core, size: u64) -> Result<Vec<u8>> {
    let size = usize::try_from(size).map_err(|_| Error::invalid("pack too large"))?;
    if size > core.opts.max_pack_bytes {
        return Err(Error::invalid("pack exceeds the size limit"));
    }
    let mut pack = Vec::with_capacity(size.min(CHUNK_BYTES));
    while pack.len() < size {
        match recv(ws, core.opts.io_timeout).await? {
            Some(Message::Binary(data)) if pack.len() + data.len() <= size => {
                pack.extend_from_slice(&data);
            }
            _ => return Err(Error::sync("malformed pack transfer")),
        }
    }
    Ok(pack)
}

async fn session(mut ws: WebSocket, core: &Arc<Core>, peer: IpAddr) -> Result<()> {
    let hello: Hello = match recv_request(&mut ws, core.opts.io_timeout).await? {
        Some(Req::Hello(hello)) => hello,
        _ => return Err(Error::sync("expected hello")),
    };
    // Subscribe before answering so that no change after the greeting is missed.
    let changes = core.opts.changes.subscribe();
    let refusal = if hello.version != PROTOCOL_VERSION {
        Some("unsupported protocol version")
    } else if !core.authorize(Some(peer), hello.token.as_deref()) {
        Some("unauthorized")
    } else {
        None
    };
    if let Some(message) = refusal {
        send_resp(
            &mut ws,
            core,
            &Resp::Error {
                message: message.into(),
            },
        )
        .await?;
        return Err(Error::sync(message));
    }
    let roots = core.blocking(|local| local.roots()).await?;
    let greeting = ServerHello {
        version: PROTOCOL_VERSION,
        device_id: core.opts.identity.device_id().to_string(),
        roots,
    };
    send_resp(&mut ws, core, &Resp::Hello(greeting)).await?;
    match hello.mode {
        Mode::Watch => watch(ws, core, changes, &hello.device_id).await,
        Mode::Sync => {
            let result = exchange(&mut ws, core, &hello.device_id).await;
            if let Err(err) = &result {
                let message = err.to_string();
                let _ = send_resp(&mut ws, core, &Resp::Error { message }).await;
            }
            result
        }
    }
}

async fn exchange(ws: &mut WebSocket, core: &Arc<Core>, device: &str) -> Result<()> {
    loop {
        let request = tokio::select! {
            () = core.cancel.cancelled() => return Ok(()),
            request = recv_request(ws, core.opts.idle_timeout) => request?,
        };
        match request {
            None => return Ok(()),
            Some(Req::Hello(_)) => return Err(Error::sync("unexpected hello")),
            Some(Req::Metas { sources }) => {
                let metas = core.blocking(move |local| local.metas(&sources)).await?;
                send_resp(ws, core, &Resp::Metas { metas }).await?;
            }
            Some(Req::Push { size }) => {
                let pack = recv_pack(ws, core, size).await?;
                let result = core.accept_pack(pack, device).await?;
                send_resp(ws, core, &Resp::Pushed(result)).await?;
            }
            Some(Req::Pull { keys, base_roots }) => {
                let pack = core.pack_for(keys, base_roots).await?;
                let size = pack.len() as u64;
                send_resp(ws, core, &Resp::Pack { size }).await?;
                for chunk in pack.chunks(CHUNK_BYTES) {
                    send(ws, core, Message::Binary(chunk.to_vec().into())).await?;
                }
            }
        }
    }
}

async fn watch(
    mut ws: WebSocket,
    core: &Arc<Core>,
    mut changes: broadcast::Receiver<Change>,
    device: &str,
) -> Result<()> {
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            () = core.cancel.cancelled() => return Ok(()),
            _ = ping.tick() => send(&mut ws, core, Message::Ping(Vec::new().into())).await?,
            frame = ws.recv() => match frame {
                None | Some(Err(_) | Ok(Message::Close(_))) => return Ok(()),
                Some(Ok(_)) => {}
            },
            change = changes.recv() => {
                let resp = match change {
                    Ok(change) if change.origin == device => continue,
                    Ok(change) => Resp::Changed { source: change.source, origin: change.origin },
                    // Missed some: tell the client to re-sync everything.
                    Err(broadcast::error::RecvError::Lagged(_)) => Resp::Changed {
                        source: SourceId::new("*"),
                        origin: String::new(),
                    },
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                };
                send_resp(&mut ws, core, &resp).await?;
            }
        }
    }
}

// ---- gRPC ---------------------------------------------------------------------------------

struct Grpc(Arc<Core>);

type GrpcStream<T> = Pin<Box<dyn Stream<Item = std::result::Result<T, Status>> + Send>>;

fn status(err: Error) -> Status {
    match err {
        Error::Integrity(_) | Error::Invalid(_) => Status::invalid_argument(err.to_string()),
        other => {
            warn!("sync request failed: {other}");
            Status::internal(other.to_string())
        }
    }
}

fn parse_roots(
    roots: &std::collections::HashMap<String, String>,
) -> Result<BTreeMap<SourceId, ContentHash>> {
    roots
        .iter()
        .map(|(source, hex)| Ok((SourceId::new(source.as_str()), ContentHash::from_hex(hex)?)))
        .collect()
}

fn hex_roots(roots: BTreeMap<SourceId, ContentHash>) -> std::collections::HashMap<String, String> {
    roots
        .into_iter()
        .map(|(source, root)| (source.as_str().to_owned(), root.to_hex()))
        .collect()
}

impl Grpc {
    fn check<T>(&self, request: &Request<T>) -> std::result::Result<(), Status> {
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip());
        let token = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if self.0.authorize(peer, token) {
            Ok(())
        } else {
            Err(Status::unauthenticated("unauthorized"))
        }
    }
}

#[tonic::async_trait]
impl AnchorSync for Grpc {
    type PullStream = GrpcStream<pb::PackChunk>;
    type WatchStream = GrpcStream<pb::Change>;

    async fn hello(
        &self,
        request: Request<pb::HelloRequest>,
    ) -> std::result::Result<GrpcResponse<pb::HelloReply>, Status> {
        self.check(&request)?;
        if request.get_ref().protocol_version != PROTOCOL_VERSION {
            return Err(Status::failed_precondition("unsupported protocol version"));
        }
        let roots = self
            .0
            .blocking(|local| local.roots())
            .await
            .map_err(status)?;
        Ok(GrpcResponse::new(pb::HelloReply {
            protocol_version: PROTOCOL_VERSION,
            device_id: self.0.opts.identity.device_id().to_string(),
            roots: hex_roots(roots),
        }))
    }

    async fn metas(
        &self,
        request: Request<pb::MetasRequest>,
    ) -> std::result::Result<GrpcResponse<pb::MetasReply>, Status> {
        self.check(&request)?;
        let sources: Vec<SourceId> = request
            .into_inner()
            .sources
            .into_iter()
            .map(SourceId::new)
            .collect();
        let metas = self
            .0
            .blocking(move |local| local.metas(&sources))
            .await
            .map_err(status)?;
        let json = serde_json::to_vec(&metas).map_err(|e| status(e.into()))?;
        Ok(GrpcResponse::new(pb::MetasReply { json }))
    }

    async fn pull(
        &self,
        request: Request<pb::PullRequest>,
    ) -> std::result::Result<GrpcResponse<Self::PullStream>, Status> {
        self.check(&request)?;
        let request = request.into_inner();
        let keys = request.keys.into_iter().map(UnitKey::new).collect();
        let base = parse_roots(&request.base_roots).map_err(status)?;
        let pack = self.0.pack_for(keys, base).await.map_err(status)?;
        let chunks: Vec<_> = pack
            .chunks(CHUNK_BYTES)
            .map(|chunk| {
                Ok(pb::PackChunk {
                    data: chunk.to_vec(),
                })
            })
            .collect();
        Ok(GrpcResponse::new(Box::pin(tokio_stream::iter(chunks))))
    }

    async fn push(
        &self,
        request: Request<Streaming<pb::PackChunk>>,
    ) -> std::result::Result<GrpcResponse<pb::PushReply>, Status> {
        self.check(&request)?;
        let origin = request
            .metadata()
            .get("x-anchor-device")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let mut stream = request.into_inner();
        let mut pack = Vec::new();
        loop {
            let next = timeout(self.0.opts.io_timeout, stream.message())
                .await
                .map_err(|_| Status::deadline_exceeded("timed out waiting for the client"))??;
            let Some(chunk) = next else { break };
            if pack.len() + chunk.data.len() > self.0.opts.max_pack_bytes {
                return Err(Status::resource_exhausted("pack exceeds the size limit"));
            }
            pack.extend_from_slice(&chunk.data);
        }
        let result = self.0.accept_pack(pack, &origin).await.map_err(status)?;
        Ok(GrpcResponse::new(pb::PushReply {
            applied: result.applied,
            stale: result.stale,
            rejected: result
                .rejected
                .iter()
                .map(|r| format!("{}: {}", r.key, r.reason))
                .collect(),
        }))
    }

    async fn watch(
        &self,
        request: Request<pb::WatchRequest>,
    ) -> std::result::Result<GrpcResponse<Self::WatchStream>, Status> {
        self.check(&request)?;
        let device = request.into_inner().device_id;
        let changes = tokio_stream::wrappers::BroadcastStream::new(self.0.opts.changes.subscribe());
        let stream = changes
            .filter_map(move |change| {
                let device = device.clone();
                async move {
                    match change {
                        Ok(c) if c.origin == device => None,
                        Ok(c) => Some(Ok(pb::Change {
                            source: c.source.as_str().to_owned(),
                            origin: c.origin,
                        })),
                        Err(_) => Some(Ok(pb::Change {
                            source: "*".to_owned(),
                            origin: String::new(),
                        })),
                    }
                }
            })
            .take_until(self.0.cancel.clone().cancelled_owned());
        Ok(GrpcResponse::new(Box::pin(stream)))
    }
}

crate::base::opaque_debug!(ServerOptions, SyncServer);
