//! End-to-end tests: an in-process server on 127.0.0.1:0 and real clients over both transports.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::base::{
    DeviceId, EdgeAssertion, EdgeKind, Hlc, NodeAssertion, NodeKind, SourceDescriptor, SourceId,
    SourceKind, TrustLevel, Unit, UnitKind, UnitVersion, keys,
};
use crate::engine::graph::ContextGraph;
use crate::engine::store::GraphStore;

use super::client::{Peer, WsPeer};
use super::identity::{DeviceIdentity, TrustPolicy};
use super::pack::tests::{dead, live, source};
use super::protocol::{Local, Mode, PROTOCOL_VERSION};
use super::store::{MemoryUnitStore, UnitStore};
use super::{
    ClientOptions, GraphUnitStore, ServerOptions, SyncClient, SyncServer, SyncTransport,
    token_from_env,
};

struct Server {
    addr: std::net::SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(store: Arc<dyn UnitStore>, token: Option<&str>, trust: TrustPolicy) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let mut options = ServerOptions::new(
            Arc::new(DeviceIdentity::generate()),
            token_from_env(Some("T"), |_| token.map(str::to_owned)),
        );
        options.trust = trust;
        let (stop, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = SyncServer::serve(listener, store, options, async {
                let _ = rx.await;
            })
            .await;
        });
        Self {
            addr,
            stop: Some(stop),
            task,
        }
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), self.task).await;
    }

    fn options(
        &self,
        transport: SyncTransport,
        token: Option<&str>,
        publish: &[&str],
        subscribe: &[&str],
    ) -> ClientOptions {
        let (scheme, path) = match transport {
            SyncTransport::WebSocket => ("ws", "/sync"),
            SyncTransport::Grpc => ("http", ""),
        };
        let ids = |names: &[&str]| names.iter().map(|n| SourceId::new(*n)).collect();
        ClientOptions {
            url: format!("{scheme}://{}{path}", self.addr),
            transport,
            token: token.map(str::to_owned),
            identity: Arc::new(DeviceIdentity::generate()),
            trust: TrustPolicy::AnyValid,
            limits: Default::default(),
            publish: ids(publish),
            subscribe: ids(subscribe),
            depths: Default::default(),
            default_depth: crate::config::Depth::Full,
            interval: Duration::from_secs(3600),
            connect_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(10),
            max_pack_bytes: 64 << 20,
        }
    }
}

fn memory(entries: Vec<super::pack::PackEntry>, sources: &[&str]) -> Arc<MemoryUnitStore> {
    let store = Arc::new(MemoryUnitStore::new());
    let descriptors: Vec<_> = sources.iter().map(|s| source(s)).collect();
    store.apply(&descriptors, entries).expect("seed");
    store
}

/// Every unit version of every namespace, for equality checks.
fn dump(store: &dyn UnitStore) -> BTreeMap<String, (bool, Option<Unit>)> {
    let mut out = BTreeMap::new();
    for source in store.sources().expect("sources") {
        for meta in store.metas(&source.id).expect("metas") {
            let (stored, unit) = store.get(&meta.key).expect("get").expect("present");
            assert_eq!(stored, meta);
            out.insert(
                format!("{}@{}@{}", meta.key, meta.hlc.raw(), meta.origin),
                (meta.deleted, unit),
            );
        }
    }
    out
}

const TRANSPORTS: [SyncTransport; 2] = [SyncTransport::WebSocket, SyncTransport::Grpc];

#[tokio::test]
async fn clients_converge_through_a_relay_including_deletions() {
    for transport in TRANSPORTS {
        let relay = memory(vec![], &[]);
        let server = Server::start(relay.clone(), None, TrustPolicy::AnyValid).await;
        let a = memory(
            vec![
                live("alpha", "x.rs", "x1", 1, "a"),
                live("alpha", "y.rs", "y1", 1, "a"),
                live("beta", "m.md", "m1", 1, "a"),
            ],
            &["alpha", "beta"],
        );
        let b = memory(vec![live("alpha", "w.rs", "w1", 2, "b")], &["alpha"]);
        let client = |store: &Arc<MemoryUnitStore>| {
            SyncClient::new(
                store.clone(),
                server.options(transport, None, &["alpha", "beta"], &[]),
            )
        };
        let (ca, cb) = (client(&a), client(&b));

        let report = ca.sync_once().await.expect("a");
        assert_eq!((report.pushed, report.pulled), (3, 0));
        let report = cb.sync_once().await.expect("b");
        assert_eq!((report.pushed, report.pulled), (1, 3));
        assert_eq!(dump(a.as_ref()).len(), 3);

        // Divergent edits: b deletes y and edits x, a edits x too (older) and adds z.
        b.apply(
            &[],
            vec![
                dead("alpha", "y.rs", 5, "b"),
                live("alpha", "x.rs", "x-b", 6, "b"),
            ],
        )
        .expect("b edits");
        a.apply(
            &[],
            vec![
                live("alpha", "x.rs", "x-a", 3, "a"),
                live("alpha", "z.rs", "z1", 4, "a"),
            ],
        )
        .expect("a edits");
        cb.sync_once().await.expect("b");
        ca.sync_once().await.expect("a");
        cb.sync_once().await.expect("b");

        let settled = dump(relay.as_ref());
        assert_eq!(dump(a.as_ref()), settled, "{transport:?}");
        assert_eq!(dump(b.as_ref()), settled, "{transport:?}");
        assert!(
            settled
                .values()
                .any(|(deleted, unit)| *deleted && unit.is_none())
        );
        assert_eq!(settled.values().filter(|(deleted, _)| !deleted).count(), 4);
        // A converged pair costs nothing.
        let report = ca.sync_once().await.expect("noop");
        assert_eq!((report.pushed, report.pulled, report.bytes), (0, 0, 0));
        server.stop().await;
    }
}

#[tokio::test]
async fn publish_and_subscribe_filters_are_honoured() {
    for transport in TRANSPORTS {
        let relay = memory(
            vec![
                live("beta", "b.md", "b1", 1, "s"),
                live("gamma", "g.md", "g1", 1, "s"),
            ],
            &["beta", "gamma"],
        );
        let server = Server::start(relay.clone(), None, TrustPolicy::AnyValid).await;
        let device = memory(
            vec![
                live("alpha", "a.rs", "a1", 1, "d"),
                live("delta", "d.rs", "d1", 1, "d"),
            ],
            &["alpha", "delta"],
        );
        let client = SyncClient::new(
            device.clone(),
            server.options(transport, None, &["alpha"], &["beta"]),
        );
        let report = client.sync_once().await.expect("sync");
        assert_eq!((report.pushed, report.pulled), (1, 1));

        let on_server: Vec<_> = relay
            .sources()
            .expect("sources")
            .into_iter()
            .map(|s| s.id.to_string())
            .collect();
        assert_eq!(on_server, ["alpha", "beta", "gamma"]);
        assert!(
            relay
                .metas(&SourceId::new("delta"))
                .expect("metas")
                .is_empty()
        );
        let on_device: Vec<_> = device
            .sources()
            .expect("sources")
            .into_iter()
            .map(|s| s.id.to_string())
            .collect();
        assert_eq!(on_device, ["alpha", "beta", "delta"]);

        // Empty publish pushes nothing; empty subscribe pulls everything.
        let idle = memory(vec![live("zeta", "z.rs", "z1", 1, "i")], &["zeta"]);
        let client = SyncClient::new(idle.clone(), server.options(transport, None, &[], &[]));
        let report = client.sync_once().await.expect("sync");
        assert_eq!((report.pushed, report.pulled), (0, 3));
        assert!(
            relay
                .metas(&SourceId::new("zeta"))
                .expect("metas")
                .is_empty()
        );
        server.stop().await;
    }
}

#[tokio::test]
async fn wrong_or_missing_tokens_are_rejected() {
    for transport in TRANSPORTS {
        let relay = memory(vec![], &[]);
        let server = Server::start(relay.clone(), Some("secret"), TrustPolicy::AnyValid).await;
        let store = memory(vec![live("alpha", "a.rs", "a1", 1, "d")], &["alpha"]);
        for token in [Some("wrong"), None] {
            let client = SyncClient::new(
                store.clone(),
                server.options(transport, token, &["alpha"], &[]),
            );
            assert!(client.sync_once().await.is_err(), "{transport:?} {token:?}");
        }
        assert!(relay.sources().expect("sources").is_empty());
        let client = SyncClient::new(
            store,
            server.options(transport, Some("secret"), &["alpha"], &[]),
        );
        assert_eq!(client.sync_once().await.expect("sync").pushed, 1);
        server.stop().await;
    }
}

#[tokio::test]
async fn tampered_and_untrusted_packs_are_rejected_without_applying_anything() {
    let relay = memory(vec![], &[]);
    let server = Server::start(relay.clone(), None, TrustPolicy::AnyValid).await;
    let identity = Arc::new(DeviceIdentity::generate());
    let sender = Local {
        store: memory(
            vec![
                live("alpha", "a.rs", "a1", 1, "d"),
                live("alpha", "b.rs", "b1", 2, "d"),
            ],
            &["alpha"],
        ),
        identity: identity.clone(),
        trust: TrustPolicy::AnyValid,
        limits: Default::default(),
    };
    let keys: Vec<_> = sender
        .store
        .metas(&SourceId::new("alpha"))
        .expect("metas")
        .into_iter()
        .map(|m| m.key)
        .collect();
    let pack = sender.build_pack(&keys, &BTreeMap::new()).expect("pack");
    let options = server.options(SyncTransport::WebSocket, None, &["alpha"], &[]);

    for index in [pack.len() - 1, pack.len() / 2] {
        let mut tampered = pack.clone();
        tampered[index] ^= 0x55;
        let mut peer = WsPeer::connect(&options).await.expect("connect");
        say_hello(&mut peer).await;
        assert!(peer.push(tampered).await.is_err(), "flip at {index}");
        assert!(relay.sources().expect("sources").is_empty());
        assert_eq!(relay.len(), 0);
    }
    // The untampered pack goes through.
    let mut peer = WsPeer::connect(&options).await.expect("connect");
    say_hello(&mut peer).await;
    assert_eq!(peer.push(pack.clone()).await.expect("push").applied, 2);
    server.stop().await;

    // A signer outside the allowlist is refused.
    let strict = memory(vec![], &[]);
    let server = Server::start(strict.clone(), None, TrustPolicy::allowlist([])).await;
    let options = server.options(SyncTransport::WebSocket, None, &["alpha"], &[]);
    let mut peer = WsPeer::connect(&options).await.expect("connect");
    say_hello(&mut peer).await;
    assert!(peer.push(pack).await.is_err());
    assert_eq!(strict.len(), 0);
    server.stop().await;
}

async fn say_hello(peer: &mut WsPeer) {
    let hello = super::protocol::Hello {
        version: PROTOCOL_VERSION,
        device_id: "d".into(),
        token: None,
        mode: Mode::Sync,
        roots: BTreeMap::new(),
    };
    peer.hello(hello).await.expect("hello");
}

fn graph_store(
    sources: &[(&str, TrustLevel)],
) -> (GraphStore, Arc<Mutex<ContextGraph>>, Arc<GraphUnitStore>) {
    let store = GraphStore::new(crate::base::Db::open_in_memory().expect("db")).expect("store");
    for (id, trust) in sources {
        store
            .upsert_source(&SourceDescriptor {
                id: SourceId::new(*id),
                kind: SourceKind::Repo,
                name: (*id).to_owned(),
                trust: *trust,
            })
            .expect("source");
    }
    let mirror = Arc::new(Mutex::new(ContextGraph::new()));
    let sink = mirror.clone();
    let unit_store = Arc::new(GraphUnitStore::new(
        store.clone(),
        Arc::new(move |outcome| sink.lock().apply(&outcome.delta)),
    ));
    (store, mirror, unit_store)
}

fn owners(source: &str, name: &str, team: &str, service: &str) -> Unit {
    let id = SourceId::new(source);
    let mut unit = Unit::new(
        keys::unit(&id, UnitKind::Ownership, name),
        id,
        UnitKind::Ownership,
    );
    let team = unit.add_node(NodeAssertion::new(NodeKind::Team, keys::team(team), team));
    let service = unit.add_node(NodeAssertion::new(
        NodeKind::Service,
        keys::service(service),
        service,
    ));
    unit.add_edge(EdgeAssertion::new(&team, EdgeKind::Owns, &service));
    unit
}

fn version(n: u64, device: &str) -> UnitVersion {
    UnitVersion::new(Hlc::new(n, 0), DeviceId::new(device))
}

fn assert_mirror(store: &GraphStore, mirror: &Mutex<ContextGraph>) {
    let (nodes, edges) = store.load_graph().expect("graph");
    let fresh = ContextGraph::from_parts(nodes, edges);
    let graph = mirror.lock();
    assert_eq!(
        (graph.node_count(), graph.edge_count()),
        (fresh.node_count(), fresh.edge_count())
    );
    for node in fresh.nodes() {
        assert_eq!(graph.node(node.id), Some(node));
    }
    for edge in fresh.edges() {
        assert_eq!(graph.edge(edge.id), Some(edge));
    }
}

#[tokio::test]
async fn graph_stores_converge_and_their_mirrors_stay_in_lockstep() {
    for transport in TRANSPORTS {
        let relay = Server::start(memory(vec![], &[]), None, TrustPolicy::AnyValid).await;
        let (a, mirror_a, unit_a) = graph_store(&[("repo", TrustLevel::SourceOfTruth)]);
        let (b, mirror_b, unit_b) = graph_store(&[("repo", TrustLevel::SourceOfTruth)]);
        a.apply_unit(&owners("repo", "o1", "payments", "api"), &version(1, "a"))
            .expect("a1");
        a.apply_unit(&owners("repo", "o2", "search", "index"), &version(2, "a"))
            .expect("a2");
        mirror_b.lock().apply(
            &b.apply_unit(&owners("repo", "o3", "infra", "db"), &version(3, "b"))
                .expect("b1")
                .delta,
        );

        let client = |store: Arc<GraphUnitStore>| {
            SyncClient::new(store, relay.options(transport, None, &["repo"], &[]))
        };
        let (ca, cb) = (client(unit_a.clone()), client(unit_b.clone()));
        ca.sync_once().await.expect("a");
        cb.sync_once().await.expect("b");
        // a deletes o2 and edits o1; b edits o3.
        let key = owners("repo", "o2", "search", "index").key;
        a.remove_unit(&key, &version(10, "a")).expect("remove");
        a.apply_unit(
            &owners("repo", "o1", "payments", "api-v2"),
            &version(11, "a"),
        )
        .expect("edit");
        mirror_b.lock().apply(
            &b.apply_unit(&owners("repo", "o3", "infra", "db-v2"), &version(12, "b"))
                .expect("edit")
                .delta,
        );
        ca.sync_once().await.expect("a");
        cb.sync_once().await.expect("b");
        ca.sync_once().await.expect("a");

        assert_eq!(
            a.snapshot().expect("a"),
            b.snapshot().expect("b"),
            "{transport:?}"
        );
        assert_eq!(
            a.unit_metas(&SourceId::new("repo")).expect("m"),
            b.unit_metas(&SourceId::new("repo")).expect("m")
        );
        assert_mirror(&b, &mirror_b);
        // Only the remote-applied part of a's graph flows through the callback.
        assert!(mirror_a.lock().node_count() > 0);
        relay.stop().await;
    }
}

#[tokio::test]
async fn local_trust_wins_over_the_peers_descriptor() {
    let relay = Server::start(memory(vec![], &[]), None, TrustPolicy::AnyValid).await;
    let (a, _, unit_a) = graph_store(&[("repo", TrustLevel::SourceOfTruth)]);
    let (b, _, unit_b) = graph_store(&[("repo", TrustLevel::ReferenceOnly)]);
    a.apply_unit(&owners("repo", "o1", "payments", "api"), &version(1, "a"))
        .expect("a1");
    let options = relay.options(SyncTransport::WebSocket, None, &["repo"], &[]);
    SyncClient::new(unit_a, options.clone())
        .sync_once()
        .await
        .expect("a");
    SyncClient::new(unit_b, options)
        .sync_once()
        .await
        .expect("b");
    assert_eq!(b.sources().expect("s")[0].trust, TrustLevel::ReferenceOnly);
    assert_eq!(a.sources().expect("s")[0].trust, TrustLevel::SourceOfTruth);
    assert_eq!(b.snapshot().expect("b").0.len(), 2);
    relay.stop().await;
}

#[tokio::test]
async fn unknown_sources_register_from_the_peers_descriptor() {
    let relay = Server::start(memory(vec![], &[]), None, TrustPolicy::AnyValid).await;
    let (a, _, unit_a) = graph_store(&[("repo", TrustLevel::SourceOfTruth)]);
    let (b, mirror_b, unit_b) = graph_store(&[]);
    a.apply_unit(&owners("repo", "o1", "payments", "api"), &version(1, "a"))
        .expect("a1");
    let options = relay.options(SyncTransport::WebSocket, None, &["repo"], &[]);
    SyncClient::new(unit_a, options.clone())
        .sync_once()
        .await
        .expect("a");
    SyncClient::new(unit_b, options)
        .sync_once()
        .await
        .expect("b");
    assert_eq!(b.sources().expect("s")[0].trust, TrustLevel::SourceOfTruth);
    assert_eq!(a.snapshot().expect("a"), b.snapshot().expect("b"));
    assert_mirror(&b, &mirror_b);
    relay.stop().await;
}

#[tokio::test]
async fn change_notifications_trigger_a_resync() {
    for transport in TRANSPORTS {
        let relay = memory(vec![], &[]);
        let server = Server::start(relay, None, TrustPolicy::AnyValid).await;
        let a = memory(vec![live("alpha", "a.rs", "a1", 1, "a")], &["alpha"]);
        let b = memory(vec![], &[]);
        let ca = SyncClient::new(a.clone(), server.options(transport, None, &["alpha"], &[]));
        let cb = SyncClient::new(b.clone(), server.options(transport, None, &[], &[]));
        let (stop, rx) = oneshot::channel::<()>();
        let runner = tokio::spawn(async move {
            cb.run(async {
                let _ = rx.await;
            })
            .await;
        });
        // Let b's initial round and watch connection settle, then publish from a.
        tokio::time::sleep(Duration::from_millis(700)).await;
        ca.sync_once().await.expect("a");
        let mut seen = false;
        for _ in 0..50 {
            if b.len() == 1 {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(seen, "{transport:?}: b never received the pushed unit");
        let _ = stop.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), runner).await;
        server.stop().await;
    }
}

/// The server terminates no TLS itself (deployments put a TLS-terminating proxy in front), so
/// what is verified here is the client side: `wss://` and `https://` URLs make the client open
/// with a TLS ClientHello (record type 0x16), while `ws://` and `http://` do not.
#[tokio::test]
async fn client_speaks_tls_for_secure_urls_and_plaintext_otherwise() {
    use tokio::io::AsyncReadExt as _;

    let cases = [
        (SyncTransport::WebSocket, "wss://", true),
        (SyncTransport::WebSocket, "ws://", false),
        (SyncTransport::Grpc, "https://", true),
        (SyncTransport::Grpc, "http://", false),
    ];
    for (transport, scheme, expect_tls) in cases {
        let probe = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = probe.local_addr().expect("addr");
        let first = tokio::spawn(async move {
            let (mut socket, _) = probe.accept().await.expect("accept");
            let mut byte = [0u8; 1];
            socket.read_exact(&mut byte).await.expect("first byte");
            byte[0]
            // Dropping the socket aborts the client's handshake.
        });
        // Reuse the option builder; only the URL and timeouts matter here.
        let dummy = Server {
            addr,
            stop: None,
            task: tokio::spawn(async {}),
        };
        let mut options = dummy.options(transport, Some("t"), &[], &[]);
        options.url = format!("{scheme}{addr}");
        options.connect_timeout = Duration::from_secs(2);
        options.io_timeout = Duration::from_secs(2);
        let client = SyncClient::new(memory(vec![], &[]), options);
        assert!(client.sync_once().await.is_err());
        let byte = tokio::time::timeout(Duration::from_secs(5), first)
            .await
            .expect("probe timed out")
            .expect("probe");
        assert_eq!(byte == 0x16, expect_tls, "{scheme}: first byte {byte:#x}");
    }
}

#[tokio::test]
async fn token_lookup_is_injectable_and_comparison_exact() {
    assert_eq!(
        token_from_env(Some("X"), |_| Some("t".into())).as_deref(),
        Some("t")
    );
    assert_eq!(token_from_env(Some("X"), |_| Some(String::new())), None);
    assert_eq!(token_from_env(None, |_| Some("t".into())), None);
    assert!(super::tokens_match("abc", "abc"));
    assert!(!super::tokens_match("abc", "abd"));
    assert!(!super::tokens_match("abc", "abcd"));
}
