//! The dashboard's HTTP API, driven through the router without a socket: routes, JSON shapes,
//! error mapping and the loopback Host / Origin checks.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use anchor::base::{SourceKind, TrustLevel};
use anchor::config::{AnchorConfig, SourceConfig};
use anchor::engine::Engine;
use anchor::engine::query::{DepthControl, QueryDeps, QueryEngine};
use anchor::engine::router::ProviderKind;
use anchor::web::{WebState, router};
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Fixture {
    app: Router,
    engine: Arc<Engine>,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("CODEOWNERS"), "* @acme/payments-platform\n").unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "/// Charges a card.\npub fn charge(amount: u64) -> u64 { validate(amount) }\nfn validate(amount: u64) -> u64 { amount }\n",
    )
    .unwrap();
    let mut config = AnchorConfig {
        data_dir: Some(dir.path().join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    let mut source = SourceConfig::new("payments-api", SourceKind::Repo, repo.to_string_lossy());
    source.trust = TrustLevel::SourceOfTruth;
    config.sources.push(source);
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    let depths: Arc<dyn DepthControl> = engine.clone();
    let query = QueryEngine::new(QueryDeps {
        store: engine.store().clone(),
        graph: engine.graph(),
        vectors: None,
        embedder: None,
        router: engine.router(),
        backlog: Some(engine.backlog()),
        graph_config: engine.config().graph.clone(),
        query_config: engine.config().query.clone(),
    })
    .with_depths(depths);
    let app = router(WebState {
        engine: engine.clone(),
        query,
    });
    Fixture {
        app,
        engine,
        _dir: dir,
    }
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|err| {
            panic!("not JSON ({err}): {}", String::from_utf8_lossy(&self.body))
        })
    }
}

async fn call(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let body = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: body.to_vec(),
    }
}

fn request(method: Method, uri: &str, host: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(host) = host {
        builder = builder.header(header::HOST, host);
    }
    builder.body(Body::empty()).unwrap()
}

async fn get(app: &Router, uri: &str) -> Reply {
    call(app, request(Method::GET, uri, Some("localhost:7890"))).await
}

async fn post(app: &Router, uri: &str) -> Reply {
    call(app, request(Method::POST, uri, Some("localhost:7890"))).await
}

fn post_json(uri: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::HOST, "localhost:7890")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn error_message(reply: &Reply) -> String {
    let body = reply.json();
    assert_eq!(
        body["error"]["code"].as_u64(),
        Some(u64::from(reply.status.as_u16())),
        "{body}"
    );
    body["error"]["message"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn the_index_page_and_health_are_served() {
    let f = fixture();
    let index = get(&f.app, "/").await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(
        index.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(String::from_utf8_lossy(&index.body).contains("<html"));

    let health = get(&f.app, "/api/health").await;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.json()["status"], "ok");
    assert_eq!(health.json()["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn only_loopback_hosts_are_answered() {
    let f = fixture();
    for host in [
        Some("localhost"),
        Some("localhost:7890"),
        Some("127.0.0.1:7890"),
        Some("[::1]:7890"),
        Some("[::1]"),
        Some("LOCALHOST:7890"),
        Some("localhost.:7890"),
        None, // HTTP/1.0 clients send no Host
    ] {
        let reply = call(&f.app, request(Method::GET, "/api/health", host)).await;
        assert_eq!(reply.status, StatusCode::OK, "{host:?}");
    }
    for host in [
        "evil.example",
        "evil.example:7890",
        "localhost.evil.example",
        "127.0.0.1.evil.example:80",
        "192.168.1.10:7890",
        "localhost:notaport",
        "0.0.0.0:7890",
    ] {
        let reply = call(&f.app, request(Method::GET, "/api/health", Some(host))).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{host}");
        assert!(error_message(&reply).contains("loopback"), "{host}");
    }
}

#[tokio::test]
async fn cross_origin_writes_are_refused_before_they_reach_a_handler() {
    let f = fixture();
    let forged = |origin: &str| {
        Request::builder()
            .method(Method::POST)
            .uri("/api/sources/payments-api/rescan")
            .header(header::HOST, "localhost:7890")
            .header(header::ORIGIN, origin)
            .body(Body::empty())
            .unwrap()
    };
    for origin in [
        "https://evil.example",
        "null",
        "http://localhost.evil.example",
        "http://evil.example/localhost",
        "file:///etc/passwd",
        "",
    ] {
        let reply = call(&f.app, forged(origin)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{origin:?}");
    }
    for origin in [
        "http://localhost:7890",
        "http://127.0.0.1:7890",
        "http://[::1]:7890",
        "https://localhost",
    ] {
        let reply = call(&f.app, forged(origin)).await;
        assert_eq!(reply.status, StatusCode::OK, "{origin:?}");
    }
}

#[tokio::test]
async fn stats_describe_the_store_the_graph_and_the_sources() {
    let f = fixture();
    let reply = get(&f.app, "/api/stats").await;
    assert_eq!(reply.status, StatusCode::OK);
    let body = reply.json();
    assert!(body["store"]["node_count"].as_u64().unwrap() > 0, "{body}");
    assert_eq!(
        body["graph"]["nodes"].as_u64(),
        body["store"]["node_count"].as_u64()
    );
    assert_eq!(body["sources"][0]["id"], "payments-api");
    assert_eq!(body["sources"][0]["trust"], "source_of_truth");
    assert_eq!(body["sources"][0]["depth"], "full");
}

#[tokio::test]
async fn search_finds_nodes_filters_by_kind_and_rejects_bad_input() {
    let f = fixture();
    let found = get(&f.app, "/api/graph/search?q=charge").await;
    assert_eq!(found.status, StatusCode::OK);
    let nodes = found.json();
    assert!(
        nodes.as_array().unwrap().iter().any(|n| n["name"]
            .as_str()
            .is_some_and(|name| name.contains("charge"))),
        "{nodes}"
    );

    let only_files = get(&f.app, "/api/graph/search?q=lib&kind=file").await;
    assert_eq!(only_files.status, StatusCode::OK);
    assert!(
        only_files
            .json()
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["kind"] == "file")
    );

    let none = get(&f.app, "/api/graph/search?q=zzzzqqqq").await;
    assert_eq!(none.json(), json!([]));

    let capped = get(&f.app, "/api/graph/search?q=charge&limit=1").await;
    assert_eq!(capped.json().as_array().unwrap().len(), 1);

    let bad_kind = get(&f.app, "/api/graph/search?q=x&kind=spaceship").await;
    assert_eq!(bad_kind.status, StatusCode::BAD_REQUEST);
    error_message(&bad_kind);

    let missing_query = get(&f.app, "/api/graph/search").await;
    assert_eq!(missing_query.status, StatusCode::BAD_REQUEST);

    // Query text that is hostile to the full-text syntax is an empty result or a clean error,
    // never a server failure.
    for q in [
        "%22unbalanced",
        "a%20AND%20(",
        "*",
        "%27%3B%20DROP%20TABLE%20nodes%3B--",
    ] {
        let reply = get(&f.app, &format!("/api/graph/search?q={q}")).await;
        assert!(
            reply.status == StatusCode::OK || reply.status == StatusCode::BAD_REQUEST,
            "{q}: {}",
            reply.status
        );
    }
}

#[tokio::test]
async fn a_node_is_returned_with_its_edges_neighbours_and_chunks() {
    let f = fixture();
    let nodes = get(&f.app, "/api/graph/search?q=charge").await.json();
    let id = nodes[0]["id"].as_i64().unwrap();
    let reply = get(&f.app, &format!("/api/graph/node/{id}")).await;
    assert_eq!(reply.status, StatusCode::OK);
    let body = reply.json();
    assert_eq!(body["node"]["id"].as_i64(), Some(id));
    for field in ["edges", "neighbors", "chunks"] {
        assert!(body[field].is_array(), "{field} in {body}");
    }

    let missing = get(&f.app, "/api/graph/node/99999999").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert!(error_message(&missing).contains("99999999"));

    let malformed = get(&f.app, "/api/graph/node/not-a-number").await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn paths_are_found_between_connected_nodes_and_404_otherwise() {
    let f = fixture();
    let file = get(&f.app, "/api/graph/search?q=lib&kind=file")
        .await
        .json();
    let symbol = get(&f.app, "/api/graph/search?q=charge&kind=symbol")
        .await
        .json();
    let (from, to) = (
        file[0]["id"].as_i64().unwrap(),
        symbol[0]["id"].as_i64().unwrap(),
    );
    let found = get(&f.app, &format!("/api/graph/path?from={from}&to={to}")).await;
    assert_eq!(
        found.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&found.body)
    );
    let body = found.json();
    assert!(body["nodes"].as_array().unwrap().len() >= 2);
    assert!(body["path"].is_object());

    let nowhere = get(&f.app, "/api/graph/path?from=99999998&to=99999999").await;
    assert_eq!(nowhere.status, StatusCode::NOT_FOUND);
    assert!(error_message(&nowhere).contains("no path"));

    let incomplete = get(&f.app, &format!("/api/graph/path?from={from}")).await;
    assert_eq!(incomplete.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_hierarchy_lists_the_organisational_nodes() {
    let f = fixture();
    let reply = get(&f.app, "/api/graph/hierarchy").await;
    assert_eq!(reply.status, StatusCode::OK);
    let tree = reply.json();
    assert!(
        tree.as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["node"]["kind"] == "service"),
        "{tree}"
    );
}

#[tokio::test]
async fn queries_are_answered_and_malformed_requests_refused() {
    let f = fixture();
    let ok = call(
        &f.app,
        post_json(
            "/api/query",
            &json!({ "query": "Who owns payments-api?", "mode": "retrieval_only" }),
        ),
    )
    .await;
    assert_eq!(
        ok.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&ok.body)
    );
    let body = ok.json();
    assert!(body["context"]["items"].is_array(), "{body}");
    assert!(body["intent"].is_object());

    let missing_field = call(&f.app, post_json("/api/query", &json!({ "text": "x" }))).await;
    assert_eq!(missing_field.status, StatusCode::UNPROCESSABLE_ENTITY);

    let not_json = call(
        &f.app,
        Request::builder()
            .method(Method::POST)
            .uri("/api/query")
            .header(header::HOST, "localhost:7890")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{ not json"))
            .unwrap(),
    )
    .await;
    assert_eq!(not_json.status, StatusCode::BAD_REQUEST);

    let wrong_type = call(
        &f.app,
        request(Method::POST, "/api/query", Some("localhost")),
    )
    .await;
    assert_eq!(wrong_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn backlog_actions_map_unknown_actions_and_ids_to_client_errors() {
    let f = fixture();
    let listing = get(&f.app, "/api/backlog").await;
    assert_eq!(listing.status, StatusCode::OK);
    assert!(listing.json()["items"].is_array());
    assert!(listing.json()["stats"].is_object());

    let unknown_action = post(&f.app, "/api/backlog/1/explode").await;
    assert_eq!(unknown_action.status, StatusCode::BAD_REQUEST);
    assert!(error_message(&unknown_action).contains("explode"));

    let unknown_item = post(&f.app, "/api/backlog/424242/resolve").await;
    assert_eq!(unknown_item.status, StatusCode::NOT_FOUND);

    let bad_id = post(&f.app, "/api/backlog/abc/resolve").await;
    assert_eq!(bad_id.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn spend_clamps_its_window_and_survives_absurd_values() {
    let f = fixture();
    for days in ["", "?days=0", "?days=7", "?days=18446744073709551615"] {
        let reply = get(&f.app, &format!("/api/spend{days}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{days}");
        let body = reply.json();
        for field in ["summary", "timeseries", "models", "recent"] {
            assert!(!body[field].is_null(), "{field} in {body}");
        }
    }
    assert_eq!(
        get(&f.app, "/api/spend?days=-3").await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn rescan_reports_the_scan_and_unknown_sources_are_not_found() {
    let f = fixture();
    let reply = post(&f.app, "/api/sources/payments-api/rescan").await;
    assert_eq!(reply.status, StatusCode::OK);
    let report = reply.json();
    assert_eq!(report["source"], "payments-api");
    assert!(report["files_seen"].as_u64().unwrap() >= 2);

    let unknown = post(&f.app, "/api/sources/nope/rescan").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert!(error_message(&unknown).contains("nope"));
}

#[tokio::test]
async fn export_downloads_a_context_pack() {
    let f = fixture();
    let reply = post(&f.app, "/api/export").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.headers[header::CONTENT_TYPE].to_str().unwrap(),
        "application/octet-stream"
    );
    let disposition = reply.headers[header::CONTENT_DISPOSITION].to_str().unwrap();
    assert!(
        disposition.starts_with("attachment; filename=\"") && disposition.contains(".contextpack"),
        "{disposition}"
    );
    assert!(reply.body.len() > 16, "a pack, not an empty body");
    // The pack verifies and re-imports.
    let path = f._dir.path().join("downloaded.contextpack");
    std::fs::write(&path, &reply.body).unwrap();
    let engine = f.engine.clone();
    tokio::task::spawn_blocking(move || engine.import_pack(&path))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn unknown_routes_and_wrong_methods_are_refused() {
    let f = fixture();
    assert_eq!(get(&f.app, "/api/nope").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        post(&f.app, "/api/health").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        get(&f.app, "/api/export").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
}
