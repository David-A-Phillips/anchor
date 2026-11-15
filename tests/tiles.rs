//! Pulling deeper tiles from a sync server: a client without a local copy of a repository holds
//! it at phone-book depth and pulls map or full depth on demand.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use anchor::base::{SourceId, SourceKind};
use anchor::config::{AnchorConfig, Depth, SourceConfig};
use anchor::engine::query::{DepthControl, QueryDeps, QueryEngine, QueryRequest, ShallowPolicy};
use anchor::engine::router::ProviderKind;
use anchor::engine::{Engine, RemoteTiles};
use anchor::sync::identity::{DeviceIdentity, TrustPolicy};
use anchor::sync::{
    ClientOptions, GraphUnitStore, ServerOptions, SyncClient, SyncServer, SyncTiles, SyncTransport,
};

const SOURCE: &str = "payments-api";

fn config(dir: &std::path::Path, policy: ShallowPolicy) -> AnchorConfig {
    let mut config = AnchorConfig {
        data_dir: Some(dir.join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    config.query.on_shallow = policy;
    config
}

/// An engine holding a small repository at full depth.
fn server_engine(dir: &std::path::Path) -> Arc<Engine> {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("CODEOWNERS"), "* @acme/payments-platform\n").unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "/// A ledger.\npub struct Ledger;\n/// Charges a card.\npub fn charge(amount: u64) -> u64 { validate(amount) }\nfn validate(amount: u64) -> u64 { amount }\n",
    )
    .unwrap();
    let mut config = config(dir, ShallowPolicy::Manual);
    config.sources.push(SourceConfig::new(
        SOURCE,
        SourceKind::Repo,
        repo.to_string_lossy(),
    ));
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    engine
}

struct Rig {
    server: Arc<Engine>,
    client: Arc<Engine>,
    options: ClientOptions,
    store: Arc<GraphUnitStore>,
    _stop: tokio::sync::oneshot::Sender<()>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn rig(policy: ShallowPolicy) -> Rig {
    let (server_dir, client_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let server = server_engine(server_dir.path());
    let served = Arc::new(GraphUnitStore::new(
        server.store().clone(),
        Arc::new(|_| {}),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let options = ServerOptions::new(Arc::new(DeviceIdentity::generate()), None);
    tokio::spawn(async move {
        let _ = SyncServer::serve(listener, served, options, async {
            let _ = rx.await;
        })
        .await;
    });

    let client = Engine::open(config(client_dir.path(), policy)).unwrap();
    let sink = client.clone();
    let depths = client.clone();
    let store = Arc::new(
        GraphUnitStore::new(
            client.store().clone(),
            Arc::new(move |outcome| sink.absorb(outcome).unwrap()),
        )
        .with_depths(Arc::new(move |source| {
            depths.depth_or_record(source, Depth::PhoneBook)
        })),
    );
    let options = ClientOptions {
        url: format!("ws://{addr}/sync"),
        transport: SyncTransport::WebSocket,
        token: None,
        identity: Arc::new(DeviceIdentity::generate()),
        trust: TrustPolicy::AnyValid,
        limits: Default::default(),
        publish: Vec::new(),
        subscribe: Vec::new(),
        depths: Default::default(),
        default_depth: Depth::PhoneBook,
        interval: Duration::from_secs(3600),
        connect_timeout: Duration::from_secs(5),
        io_timeout: Duration::from_secs(10),
        max_pack_bytes: 64 << 20,
    };
    Rig {
        server,
        client,
        options,
        store,
        _stop: stop,
        _dirs: (server_dir, client_dir),
    }
}

impl Rig {
    async fn sync(&self) {
        SyncClient::new(self.store.clone(), self.options.clone())
            .sync_once()
            .await
            .unwrap();
    }

    fn install_remote(&self) {
        self.client.set_remote(Arc::new(SyncTiles::new(
            self.store.clone(),
            self.options.clone(),
            tokio::runtime::Handle::current(),
        )));
    }

    async fn deepen(&self, depth: Depth) -> anchor::Result<()> {
        let client = self.client.clone();
        tokio::task::spawn_blocking(move || {
            client.deepen(&SourceId::new(SOURCE), depth).map(|_| ())
        })
        .await
        .unwrap()
    }

    fn keys(&self) -> Vec<String> {
        self.client
            .store()
            .snapshot()
            .unwrap()
            .0
            .into_keys()
            .collect()
    }

    fn queries(&self) -> QueryEngine {
        let depths: Arc<dyn DepthControl> = self.client.clone();
        QueryEngine::new(QueryDeps {
            store: self.client.store().clone(),
            graph: self.client.graph(),
            vectors: None,
            embedder: None,
            router: self.client.router(),
            backlog: Some(self.client.backlog()),
            graph_config: self.client.config().graph.clone(),
            query_config: self.client.config().query.clone(),
        })
        .with_depths(depths)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_at_phone_book_depth_brings_the_service_and_its_owners_only() {
    let rig = rig(ShallowPolicy::Manual).await;
    rig.sync().await;
    let id = SourceId::new(SOURCE);
    assert_eq!(rig.client.depth(&id), Depth::PhoneBook);
    assert_eq!(rig.client.shallow_sources(), vec![(id, Depth::PhoneBook)]);
    let server_nodes = rig.server.store().stats().unwrap().node_count;
    let client_nodes = rig.client.store().stats().unwrap().node_count;
    assert_eq!(client_nodes, 2, "service and owner team");
    assert!(server_nodes >= 3 * client_nodes, "{server_nodes}");
    // The whole unit set is not on the client: files were not even requested.
    let units = rig
        .client
        .store()
        .unit_metas(&SourceId::new(SOURCE))
        .unwrap();
    assert!(
        units.iter().all(|m| !m.key.as_str().contains("file")),
        "{units:?}"
    );

    let answer = rig
        .queries()
        .query(QueryRequest::new("Who owns the payments-api service?"))
        .await
        .unwrap();
    assert!(answer.answer.unwrap().contains("payments-platform"));
    assert_eq!(answer.shallow.len(), 1);
    assert_eq!(answer.shallow[0].status, "manual");
    assert_eq!(answer.shallow[0].depth, Depth::PhoneBook);
}

#[tokio::test(flavor = "multi_thread")]
async fn deepening_pulls_map_then_full_and_converges_on_the_server() {
    let rig = rig(ShallowPolicy::Manual).await;
    rig.sync().await;
    rig.install_remote();

    rig.deepen(Depth::Map).await.unwrap();
    let keys = rig.keys();
    assert!(keys.iter().any(|k| k.contains("lib.rs")), "{keys:?}");
    assert!(keys.iter().any(|k| k.contains("Ledger")), "{keys:?}");
    assert!(!keys.iter().any(|k| k.contains("charge")), "{keys:?}");
    assert_eq!(rig.client.depth(&SourceId::new(SOURCE)), Depth::Map);
    // The graph in memory follows the store.
    let nodes = rig.client.store().stats().unwrap().node_count as usize;
    assert_eq!(rig.client.graph().read().node_count(), nodes);

    rig.deepen(Depth::Full).await.unwrap();
    assert_eq!(rig.client.depth(&SourceId::new(SOURCE)), Depth::Full);
    assert!(rig.client.shallow_sources().is_empty());
    assert_eq!(
        rig.client.store().snapshot().unwrap(),
        rig.server.store().snapshot().unwrap()
    );
    let nodes = rig.client.store().stats().unwrap().node_count as usize;
    assert_eq!(rig.client.graph().read().node_count(), nodes);
}

#[tokio::test(flavor = "multi_thread")]
async fn block_policy_pulls_the_tile_to_answer() {
    let rig = rig(ShallowPolicy::Block).await;
    rig.sync().await;
    rig.install_remote();
    let response = rig
        .queries()
        .query(QueryRequest::new("Where is charge defined?"))
        .await
        .unwrap();
    assert!(
        response
            .context
            .items
            .iter()
            .any(|item| item.node.key.ends_with("#charge")),
        "{:?}",
        response.context.items
    );
    assert_eq!(response.shallow[0].status, "deepened");
    assert_eq!(rig.client.depth(&SourceId::new(SOURCE)), Depth::Full);
}

#[tokio::test(flavor = "multi_thread")]
async fn deepening_without_a_server_says_so() {
    let rig = rig(ShallowPolicy::Manual).await;
    rig.sync().await;
    let err = rig.deepen(Depth::Full).await.unwrap_err().to_string();
    assert!(
        err.contains("not on this machine and no sync server is configured"),
        "{err}"
    );
    assert_eq!(rig.client.depth(&SourceId::new(SOURCE)), Depth::PhoneBook);
}

#[test]
fn remote_trait_is_object_safe() {
    fn _check(_: Arc<dyn RemoteTiles>) {}
}
