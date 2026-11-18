//! The embedded web dashboard (Axum), served on `http://localhost:7890` by default.
//!
//! JSON API under `/api`, a single-page front end at `/`. The dashboard is unauthenticated, so
//! it binds to loopback by default and rejects requests whose `Host` header is not a loopback
//! name (a DNS-rebinding defence).

mod config;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::backlog::ListFilter;
use crate::base::{Error, NodeId, NodeKind, Result, SourceId};
use crate::engine::Engine;
use crate::engine::graph::{Limits, Pathfinder, TraversalProfile};
use crate::engine::query::{QueryEngine, QueryRequest};
use crate::metrics::{Bucket, TimeRange};

pub use config::WebConfig;

/// Shared state of the dashboard.
#[derive(Clone)]
pub struct WebState {
    /// The engine.
    pub engine: Arc<Engine>,
    /// The query engine.
    pub query: QueryEngine,
}

struct ApiError(StatusCode, String);

impl From<Error> for ApiError {
    fn from(err: Error) -> Self {
        let status = match err {
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Invalid(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, err.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({ "error": { "code": self.0.as_u16(), "message": self.1 } })),
        )
            .into_response()
    }
}

type Api = std::result::Result<Json<Value>, ApiError>;

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> std::result::Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .map_err(ApiError::from)
}

fn to_json<T: serde::Serialize>(value: &T) -> Api {
    serde_json::to_value(value)
        .map(Json)
        .map_err(|err| ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

async fn stats(State(state): State<WebState>) -> Api {
    let engine = state.engine.clone();
    let stats = blocking(move || {
        engine.store().catch_up()?;
        engine.store().stats()
    })
    .await?;
    let (nodes, edges, memory) = {
        let graph = state.engine.graph();
        let graph = graph.read();
        (
            graph.node_count(),
            graph.edge_count(),
            graph.memory_estimate_bytes(),
        )
    };
    let sources: Vec<Value> = state
        .engine
        .config()
        .sources
        .iter()
        .map(|s| json!({ "id": s.id, "kind": s.kind, "trust": s.trust, "name": s.display_name(), "enabled": s.enabled, "depth": state.engine.depth(&s.id) }))
        .collect();
    Ok(Json(json!({
        "store": stats,
        "graph": { "nodes": nodes, "edges": edges, "memory_bytes": memory },
        "latency": state.engine.latency_stats(),
        "sources": sources,
    })))
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    kind: Option<String>,
    limit: Option<usize>,
}

async fn search(State(state): State<WebState>, Query(params): Query<SearchParams>) -> Api {
    let kinds: Vec<NodeKind> = match &params.kind {
        Some(kind) if !kind.is_empty() => vec![kind.parse::<NodeKind>().map_err(ApiError::from)?],
        _ => Vec::new(),
    };
    let limit = params.limit.unwrap_or(25).min(200);
    let engine = state.engine.clone();
    let nodes = blocking(move || {
        let hits = engine.store().search_nodes(&params.q, &kinds, limit)?;
        let ids: Vec<NodeId> = hits.iter().map(|hit| hit.node).collect();
        engine.store().nodes(&ids)
    })
    .await?;
    to_json(&nodes)
}

async fn node(State(state): State<WebState>, Path(id): Path<i64>) -> Api {
    let engine = state.engine.clone();
    let detail = blocking(move || {
        let store = engine.store();
        let node = store
            .node(NodeId(id))?
            .ok_or_else(|| Error::not_found(format!("node {id}")))?;
        let edges = store.edges_of(NodeId(id), 500)?;
        let mut neighbours = std::collections::BTreeSet::new();
        for edge in &edges {
            neighbours.insert(edge.from);
            neighbours.insert(edge.to);
        }
        let neighbours = store.nodes(&neighbours.into_iter().collect::<Vec<_>>())?;
        let chunks = store.chunks_of(NodeId(id))?;
        Ok(json!({ "node": node, "edges": edges, "neighbors": neighbours, "chunks": chunks }))
    })
    .await?;
    Ok(Json(detail))
}

#[derive(Deserialize)]
struct PathParams {
    from: i64,
    to: i64,
}

async fn path(State(state): State<WebState>, Query(params): Query<PathParams>) -> Api {
    let engine = state.engine.clone();
    blocking(move || engine.store().catch_up()).await?;
    let config = state.engine.config().graph.clone();
    let found = {
        let graph = state.engine.graph();
        let graph = graph.read();
        Pathfinder::new(&graph, &config).shortest_path(
            NodeId(params.from),
            NodeId(params.to),
            &TraversalProfile::general(),
            Limits::from(&config),
        )
    };
    let Some(found) = found else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "no path within the configured limits".into(),
        ));
    };
    let engine = state.engine.clone();
    let ids = found.nodes.clone();
    let nodes = blocking(move || engine.store().nodes(&ids)).await?;
    Ok(Json(json!({ "path": found, "nodes": nodes })))
}

async fn hierarchy(State(state): State<WebState>) -> Api {
    let engine = state.engine.clone();
    let tree = blocking(move || {
        let store = engine.store();
        let mut out = Vec::new();
        for kind in NodeKind::HIERARCHY {
            for node in store.nodes_by_kind(*kind, 2000, 0)? {
                let edges = store.edges_of(node.id, 500)?;
                out.push(json!({ "node": node, "edges": edges }));
            }
        }
        Ok(out)
    })
    .await?;
    to_json(&tree)
}

async fn run_query(State(state): State<WebState>, Json(request): Json<QueryRequest>) -> Api {
    let response = state.query.query(request).await.map_err(ApiError::from)?;
    to_json(&response)
}

async fn backlog(State(state): State<WebState>) -> Api {
    let engine = state.engine.clone();
    let (items, stats) = blocking(move || {
        let backlog = engine.backlog();
        Ok((backlog.list(&ListFilter::default())?, backlog.stats()?))
    })
    .await?;
    Ok(Json(json!({ "items": items, "stats": stats })))
}

async fn backlog_action(
    State(state): State<WebState>,
    Path((id, action)): Path<(i64, String)>,
) -> Api {
    let engine = state.engine.clone();
    blocking(move || {
        let backlog = engine.backlog();
        match action.as_str() {
            "resolve" => backlog.resolve(id, "resolved from the dashboard"),
            "dismiss" => backlog.dismiss(id, "dismissed from the dashboard"),
            "reopen" => backlog.reopen(id),
            "retry" => backlog.retry(id),
            other => Err(Error::invalid(format!("unknown backlog action `{other}`"))),
        }
    })
    .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SpendParams {
    days: Option<u64>,
}

async fn spend(State(state): State<WebState>, Query(params): Query<SpendParams>) -> Api {
    let engine = state.engine.clone();
    let days = params.days.unwrap_or(30).clamp(1, 3650);
    let data = blocking(move || {
        let ledger = engine.ledger();
        ledger.flush()?;
        let range = TimeRange::last(Duration::from_secs(days * 86_400));
        Ok(json!({
            "summary": ledger.summary(range)?,
            "timeseries": ledger.timeseries(Bucket::Day, range)?,
            "models": ledger.by_model(range)?,
            "recent": ledger.recent(50)?,
        }))
    })
    .await?;
    Ok(Json(data))
}

async fn rescan(State(state): State<WebState>, Path(id): Path<String>) -> Api {
    let engine = state.engine.clone();
    let report = blocking(move || engine.scan_source(&SourceId::new(id))).await?;
    to_json(&report)
}

async fn export(State(state): State<WebState>) -> std::result::Result<Response, ApiError> {
    let engine = state.engine.clone();
    let (name, bytes) = blocking(move || {
        let path = engine.export_pack(None)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok((name, std::fs::read(&path)?))
    })
    .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}

async fn index() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

/// Whether `authority` (`host`, `host:port`, `[v6]` or `[v6]:port`) names this machine. Host names
/// are case-insensitive and may carry a trailing root dot.
fn is_loopback_authority(authority: &str) -> bool {
    let name = authority
        .rsplit_once(':')
        .map_or(authority, |(name, port)| {
            if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
                name
            } else {
                authority
            }
        });
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    matches!(name.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "")
}

/// Whether an `Origin` header value (`scheme://authority`) is a loopback origin. `null` (sandboxed
/// frames, some redirects) and anything unparseable are not.
fn is_loopback_origin(origin: &str) -> bool {
    origin.split_once("://").is_some_and(|(scheme, authority)| {
        matches!(scheme, "http" | "https")
            && !authority.is_empty()
            && !authority.contains('/')
            && is_loopback_authority(authority)
    })
}

/// Rejects requests addressed to a non-loopback host name (a page on another origin must not be
/// able to reach the dashboard by pointing a DNS name at 127.0.0.1) and requests that a page on
/// another origin sent to the loopback address itself (the `Origin` header, which browsers always
/// attach to cross-origin writes, must be loopback when present: the state-changing endpoints
/// would otherwise be open to cross-site request forgery from any web page the user visits).
async fn loopback_only(request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let origin_ok = request
        .headers()
        .get(header::ORIGIN)
        .is_none_or(|origin| origin.to_str().is_ok_and(is_loopback_origin));
    if is_loopback_authority(host) && origin_ok {
        next.run(request).await
    } else {
        ApiError(
            StatusCode::FORBIDDEN,
            "the dashboard only answers loopback host names and loopback origins".into(),
        )
        .into_response()
    }
}

#[derive(Deserialize)]
struct ImpactParams {
    /// Comma-separated files, symbols, keys or names.
    targets: String,
    /// Comma-separated dependent kinds to keep.
    kinds: Option<String>,
    limit: Option<usize>,
}

fn comma_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

async fn impact(State(state): State<WebState>, Query(params): Query<ImpactParams>) -> Api {
    let targets = comma_list(&params.targets);
    let options = crate::engine::impact::ImpactOptions {
        limit: params.limit.unwrap_or(50).clamp(1, 500),
        kinds: comma_list(params.kinds.as_deref().unwrap_or(""))
            .iter()
            .map(|kind| kind.parse::<NodeKind>())
            .collect::<Result<_>>()
            .map_err(ApiError::from)?,
        ..Default::default()
    };
    let engine = state.engine.clone();
    let report = blocking(move || engine.impact(&targets, &options)).await?;
    to_json(&report)
}

#[derive(Deserialize)]
struct HotspotParams {
    kind: Option<String>,
    limit: Option<usize>,
    min_dependents: Option<usize>,
}

async fn hotspots(State(state): State<WebState>, Query(params): Query<HotspotParams>) -> Api {
    let options = crate::engine::impact::HotspotOptions {
        kind: match params.kind.as_deref() {
            Some(kind) if !kind.is_empty() => kind.parse::<NodeKind>().map_err(ApiError::from)?,
            _ => NodeKind::Service,
        },
        limit: params.limit.unwrap_or(20).clamp(1, 200),
        min_dependents: params.min_dependents.unwrap_or(1).max(1),
    };
    let engine = state.engine.clone();
    let report = blocking(move || engine.hotspots(&options)).await?;
    to_json(&report)
}

/// Builds the dashboard router.
pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/stats", get(stats))
        .route("/api/graph/search", get(search))
        .route("/api/graph/node/{id}", get(node))
        .route("/api/graph/path", get(path))
        .route("/api/graph/hierarchy", get(hierarchy))
        .route("/api/impact", get(impact))
        .route("/api/hotspots", get(hotspots))
        .route("/api/query", post(run_query))
        .route("/api/backlog", get(backlog))
        .route("/api/backlog/{id}/{action}", post(backlog_action))
        .route("/api/spend", get(spend))
        .route("/api/sources/{id}/rescan", post(rescan))
        .route("/api/export", post(export))
        .layer(middleware::from_fn(loopback_only))
        .with_state(state)
}

/// Serves the dashboard on the configured address until the task is cancelled.
pub async fn serve(engine: Arc<Engine>, query: QueryEngine) -> Result<()> {
    let address = format!("{}:{}", engine.config().web.bind, engine.config().web.port);
    let listener = tokio::net::TcpListener::bind(&address).await?;
    axum::serve(listener, router(WebState { engine, query })).await?;
    Ok(())
}

crate::base::opaque_debug!(WebState);
