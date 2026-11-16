//! A Model Context Protocol server (`anchor mcp`): the standard interface through which agent
//! hosts (Claude Code, Claude Desktop, Cursor and others) use Anchor.
//!
//! The transport is stdio: one JSON-RPC 2.0 message per line on stdin, one per line on stdout.
//! Nothing else is ever written to stdout; logs go to stderr.
//!
//! Tools:
//!
//! * `ask`: a question in; the answer, the evidence with trust levels, conflicts, gaps and
//!   shallow areas out. No model is called unless `explain` is set.
//! * `search`: entities by name or alias.
//! * `get`: one entity with its attributes, relationships and text.
//! * `path`: the most trustworthy path between two entities.
//! * `why`: the trail from a file, line range or symbol to the commits, pull requests, review
//!   decisions, tickets and pages that explain it.
//! * `deepen`: ingest a shallow source more deeply, then ask again.
//! * `status`: what is indexed and at which depth.

pub mod install;

use std::sync::Arc;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::base::{Error, NodeId, NodeKey, NodeKind, Result, SourceId};
use crate::config::Depth;
use crate::engine::Engine;
use crate::engine::graph::{Limits, Pathfinder, TraversalProfile};
use crate::engine::query::{QueryEngine, QueryRequest, QueryResponse};
use crate::engine::store::NodeRecord;

/// Protocol revisions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const SNIPPET_CHARS: usize = 600;
const MAX_EDGES: usize = 60;

const INSTRUCTIONS: &str = "Anchor is a local knowledge graph of this organisation's code, docs, \
tickets, reviews and org structure. Use `ask` for questions such as who owns something, why a \
decision was made, or what depends on what; every piece of evidence carries a trust level \
(`source_of_truth` outranks `reference_only`) and conflicts are reported with the winning claim. \
If a response lists `shallow` sources, the answer rests on partially ingested data: call `deepen` \
for that source and ask again.";

/// The MCP server.
#[derive(Clone)]
pub struct McpServer {
    engine: Arc<Engine>,
    query: QueryEngine,
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    })
}

/// The tool catalogue returned by `tools/list`.
pub fn tools() -> Vec<Value> {
    vec![
        tool(
            "ask",
            "Answer a question from the knowledge graph: ownership (who owns or maintains X), \
             justification (why was X decided, dismissed or approved), dependencies and \
             relationships (what uses X, which team or portfolio X belongs to), definitions. \
             Returns the answer when the graph holds one, the evidence with trust levels, \
             conflicting claims with the authoritative winner, gaps, and `shallow` sources that \
             are only partially ingested. Fast: no model is called unless `explain` is true.",
            json!({
                "question": { "type": "string", "description": "The question, in plain language. Name the entity exactly when you can (service slug, file path, API name, ticket id)." },
                "top_k": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Maximum evidence items (default 12)." },
                "explain": { "type": "boolean", "description": "Let Anchor's configured model write the answer when the graph cannot answer directly. Slower; usually unnecessary since you can read the evidence yourself." },
            }),
            &["question"],
        ),
        tool(
            "search",
            "Find entities by name or alias: services, teams, people, components, domains, \
             portfolios, API specs and endpoints, files, symbols, wiki pages, tickets, pull \
             requests. Returns keys usable with `get` and `path`.",
            json!({
                "query": { "type": "string", "description": "Name, alias or prefix." },
                "kind": { "type": "string", "description": "Restrict to one kind, e.g. `service`, `team`, `person`, `file`, `symbol`, `wiki_page`, `ticket`." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum results (default 20)." },
            }),
            &["query"],
        ),
        tool(
            "get",
            "Everything known about one entity: attributes, the sources asserting it and their \
             trust, its relationships (owners, containers, dependencies, mentions) and its text.",
            json!({
                "entity": { "type": "string", "description": "A key from `search` or `ask` (e.g. `service:billing-api`), or a name." },
            }),
            &["entity"],
        ),
        tool(
            "path",
            "The most trustworthy chain of relationships between two entities, e.g. from a file \
             to the team that owns it, or from a service to its portfolio. Paths through \
             `reference_only` claims are used only when no authoritative path exists.",
            json!({
                "from": { "type": "string", "description": "Key or name of the start entity." },
                "to": { "type": "string", "description": "Key or name of the end entity." },
            }),
            &["from", "to"],
        ),
        tool(
            "why",
            "Explain why code is the way it is (a deeper git blame). Blames the given lines, then \
             follows each responsible commit to the pull request that carried it, the review \
             decisions and threads on it (including dismissed ones and why they were dismissed), \
             the tickets it tracks with their status and blockers, and wiki pages discussing \
             them. Reports where the trail ends (e.g. a commit with no pull request). Use this \
             before changing code whose purpose is unclear.",
            json!({
                "target": { "type": "string", "description": "`path`, `path:line`, `path:start-end` (path absolute, or relative to a repository; prefix `source:` when the same path exists in several), or a symbol name." },
            }),
            &["target"],
        ),
        tool(
            "deepen",
            "Ingest a shallow source more deeply (phone_book -> map -> full). Call this when \
             `ask` reports the source under `shallow`, then ask again. May take seconds to \
             minutes for a large repository.",
            json!({
                "source": { "type": "string", "description": "Source id as listed under `shallow` or by `status`." },
                "depth": { "type": "string", "enum": ["map", "full"], "description": "Target depth (default `full`)." },
            }),
            &["source"],
        ),
        tool(
            "status",
            "What Anchor has indexed: sources with their kind, trust and depth, and entity counts.",
            json!({}),
            &[],
        ),
    ]
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

fn string_arg<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::invalid(format!("missing argument `{name}`")))
}

fn usize_arg(args: &Map<String, Value>, name: &str) -> Option<usize> {
    args.get(name)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

fn brief(node: &NodeRecord) -> Value {
    json!({ "key": node.key, "kind": node.kind, "name": node.name, "trust": node.trust })
}

/// A compact rendering of a query response: what an agent needs, without row ids and timings.
pub fn render_answer(response: &QueryResponse) -> Value {
    let evidence: Vec<Value> = response
        .context
        .items
        .iter()
        .map(|item| {
            let mut out = Map::new();
            out.insert("key".into(), json!(item.node.key));
            out.insert("kind".into(), json!(item.node.kind));
            out.insert("name".into(), json!(item.node.name));
            out.insert("trust".into(), json!(item.node.trust));
            out.insert("role".into(), json!(item.role));
            let via = item.path_text();
            if !via.is_empty() {
                out.insert("via".into(), json!(via));
            }
            if let Some(snippet) = item.snippets.first() {
                out.insert("text".into(), json!(clip(&snippet.text, SNIPPET_CHARS)));
                out.insert("text_source".into(), json!(snippet.source));
                out.insert("text_trust".into(), json!(snippet.trust));
            }
            Value::Object(out)
        })
        .collect();
    let conflicts: Vec<Value> = response
        .context
        .conflicts
        .iter()
        .map(|conflict| {
            json!({
                "question": conflict.question,
                "winner": conflict.winner.key,
                "overridden": conflict.overridden.iter().map(|n| n.key.clone()).collect::<Vec<_>>(),
                "explanation": conflict.explanation,
            })
        })
        .collect();
    json!({
        "answer": response.answer,
        "confidence": response.confidence,
        "intent": response.intent.kind,
        "evidence": evidence,
        "conflicts": conflicts,
        "shallow": response.shallow,
        "gaps": response.gaps,
    })
}

impl McpServer {
    /// A server over an open engine.
    pub fn new(engine: Arc<Engine>, query: QueryEngine) -> Self {
        Self { engine, query }
    }

    fn resolve(&self, reference: &str) -> Result<NodeRecord> {
        let store = self.engine.store();
        if let Ok(id) = reference.parse::<i64>()
            && let Some(node) = store.node(NodeId(id))?
        {
            return Ok(node);
        }
        if let Some(node) = store.node_by_key(&NodeKey::new(reference))? {
            return Ok(node);
        }
        // Several entities often share a name (a service, its API and its component): prefer the
        // best-scoring hit, and among equals the kind people usually mean.
        const PREFERRED: &[NodeKind] = &[
            NodeKind::Service,
            NodeKind::Team,
            NodeKind::Person,
            NodeKind::Component,
            NodeKind::Domain,
            NodeKind::Portfolio,
            NodeKind::ApiSpec,
        ];
        let hits = store.search_nodes(reference, &[], 10)?;
        let best = hits.first().map_or(0.0, |hit| hit.score);
        let ids: Vec<NodeId> = hits
            .iter()
            .filter(|hit| hit.score >= best)
            .map(|hit| hit.node)
            .collect();
        store
            .nodes(&ids)?
            .into_iter()
            .min_by_key(|node| {
                PREFERRED
                    .iter()
                    .position(|kind| *kind == node.kind)
                    .unwrap_or(PREFERRED.len())
            })
            .ok_or_else(|| Error::not_found(format!("no entity matches `{reference}`")))
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&McpServer) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let server = self.clone();
        tokio::task::spawn_blocking(move || f(&server)).await?
    }

    async fn ask(&self, args: &Map<String, Value>) -> Result<Value> {
        let mut request = QueryRequest::new(string_arg(args, "question")?);
        request.top_k = usize_arg(args, "top_k").map(|k| k.clamp(1, 50));
        request.explain = args
            .get("explain")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let response = self.query.query(request).await?;
        Ok(render_answer(&response))
    }

    async fn search(&self, args: &Map<String, Value>) -> Result<Value> {
        let query = string_arg(args, "query")?.to_owned();
        let kinds: Vec<NodeKind> = match args.get("kind").and_then(Value::as_str) {
            Some(kind) if !kind.is_empty() => vec![kind.parse::<NodeKind>()?],
            _ => Vec::new(),
        };
        let limit = usize_arg(args, "limit").unwrap_or(20).clamp(1, 100);
        self.blocking(move |server| {
            let store = server.engine.store();
            let hits = store.search_nodes(&query, &kinds, limit)?;
            let mut results = Vec::new();
            for hit in hits {
                if let Some(node) = store.node(hit.node)? {
                    let mut entry = brief(&node);
                    entry["score"] = json!(hit.score);
                    results.push(entry);
                }
            }
            Ok(json!({ "results": results }))
        })
        .await
    }

    async fn get(&self, args: &Map<String, Value>) -> Result<Value> {
        let reference = string_arg(args, "entity")?.to_owned();
        self.blocking(move |server| {
            let store = server.engine.store();
            let node = server.resolve(&reference)?;
            let edges = store.edges_of(node.id, 500)?;
            let mut others: Vec<NodeId> = edges
                .iter()
                .map(|edge| if edge.from == node.id { edge.to } else { edge.from })
                .collect();
            others.sort_unstable();
            others.dedup();
            let neighbours: std::collections::HashMap<NodeId, NodeRecord> = store
                .nodes(&others)?
                .into_iter()
                .map(|n| (n.id, n))
                .collect();
            let total = edges.len();
            let relationships: Vec<Value> = edges
                .iter()
                .take(MAX_EDGES)
                .filter_map(|edge| {
                    let outgoing = edge.from == node.id;
                    let other = neighbours.get(if outgoing { &edge.to } else { &edge.from })?;
                    Some(json!({
                        "relation": edge.kind,
                        "direction": if outgoing { "out" } else { "in" },
                        "entity": brief(other),
                        "trust": edge.trust,
                        "sources": edge.claims.iter().map(|c| c.source.to_string()).collect::<std::collections::BTreeSet<_>>(),
                    }))
                })
                .collect();
            let text: Vec<Value> = store
                .chunks_of(node.id)?
                .iter()
                .take(8)
                .map(|chunk| {
                    json!({
                        "kind": chunk.kind,
                        "source": chunk.source,
                        "trust": chunk.trust,
                        "text": clip(&chunk.text, 1200),
                    })
                })
                .collect();
            Ok(json!({
                "key": node.key,
                "kind": node.kind,
                "name": node.name,
                "aliases": node.aliases,
                "trust": node.trust,
                "sources": node.sources,
                "attributes": node.attrs,
                "relationships": relationships,
                "relationships_total": total,
                "text": text,
            }))
        })
        .await
    }

    async fn path(&self, args: &Map<String, Value>) -> Result<Value> {
        let from = string_arg(args, "from")?.to_owned();
        let to = string_arg(args, "to")?.to_owned();
        self.blocking(move |server| {
            let from = server.resolve(&from)?;
            let to = server.resolve(&to)?;
            let config = server.engine.config().graph.clone();
            let found = {
                let graph = server.engine.graph();
                let graph = graph.read();
                Pathfinder::new(&graph, &config).shortest_path(
                    from.id,
                    to.id,
                    &TraversalProfile::general(),
                    Limits::from(&config),
                )
            };
            let Some(found) = found else {
                return Ok(json!({
                    "found": false,
                    "from": brief(&from),
                    "to": brief(&to),
                    "detail": "no path within the configured hop and cost limits",
                }));
            };
            let nodes: std::collections::HashMap<NodeId, NodeRecord> = server
                .engine
                .store()
                .nodes(&found.nodes)?
                .into_iter()
                .map(|n| (n.id, n))
                .collect();
            let node_at = |index: usize| {
                found
                    .nodes
                    .get(index)
                    .and_then(|id| nodes.get(id))
                    .map_or(Value::Null, brief)
            };
            let steps: Vec<Value> = found
                .steps
                .iter()
                .enumerate()
                .map(|(index, step)| {
                    json!({
                        "from": node_at(index),
                        "relation": step.kind,
                        "direction": step.direction,
                        "trust": step.trust,
                        "to": node_at(index + 1),
                    })
                })
                .collect();
            Ok(json!({
                "found": true,
                "steps": steps,
                "reference_only_elements": found.cost.reference_elements,
                "cost": found.cost.weighted,
            }))
        })
        .await
    }

    async fn deepen(&self, args: &Map<String, Value>) -> Result<Value> {
        let source = SourceId::new(string_arg(args, "source")?);
        let depth = match args.get("depth").and_then(Value::as_str) {
            Some(depth) => depth.parse::<Depth>()?,
            None => Depth::Full,
        };
        let report = self
            .blocking(move |server| server.engine.deepen(&source, depth))
            .await?;
        let embedded = self.engine.embed_pending().await?;
        self.engine.flush()?;
        Ok(json!({
            "source": report.source,
            "depth": report.depth,
            "files_seen": report.files_seen,
            "units_applied": report.units_applied,
            "chunks_embedded": embedded,
            "duration_ms": report.duration_ms,
        }))
    }

    async fn status(&self) -> Result<Value> {
        self.blocking(|server| {
            let engine = &server.engine;
            let stats = engine.store().stats()?;
            let mut sources = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for source in &engine.config().sources {
                seen.insert(source.id.clone());
                sources.push(json!({
                    "id": source.id,
                    "kind": source.kind,
                    "trust": source.trust,
                    "enabled": source.enabled,
                    "depth": engine.depth(&source.id),
                }));
            }
            for source in engine.store().sources()? {
                if seen.insert(source.id.clone()) {
                    sources.push(json!({
                        "id": source.id,
                        "kind": source.kind,
                        "trust": source.trust,
                        "remote": true,
                        "depth": engine.depth(&source.id),
                    }));
                }
            }
            Ok(json!({ "sources": sources, "entities": stats.nodes, "store": stats }))
        })
        .await
    }

    /// Runs one tool. `Err` means the call failed; the caller reports it as a tool error.
    pub async fn call_tool(&self, name: &str, args: &Map<String, Value>) -> Result<Value> {
        match name {
            "ask" => self.ask(args).await,
            "search" => self.search(args).await,
            "get" => self.get(args).await,
            "path" => self.path(args).await,
            "why" => {
                let target = string_arg(args, "target")?.to_owned();
                let report = self
                    .blocking(move |server| server.engine.why(&target))
                    .await?;
                Ok(serde_json::to_value(report)?)
            }
            "deepen" => self.deepen(args).await,
            "status" => self.status().await,
            other => Err(Error::not_found(format!("unknown tool `{other}`"))),
        }
    }

    async fn dispatch(
        &self,
        method: &str,
        params: &Value,
    ) -> std::result::Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                let version = requested
                    .filter(|version| PROTOCOL_VERSIONS.contains(version))
                    .unwrap_or(PROTOCOL_VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "anchor", "title": "Anchor", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or((INVALID_PARAMS, "missing tool name".to_owned()))?;
                if !tools().iter().any(|tool| tool["name"] == name) {
                    return Err((INVALID_PARAMS, format!("unknown tool `{name}`")));
                }
                let empty = Map::new();
                let args = match params.get("arguments") {
                    None | Some(Value::Null) => &empty,
                    Some(Value::Object(args)) => args,
                    Some(_) => {
                        return Err((INVALID_PARAMS, "`arguments` must be an object".to_owned()));
                    }
                };
                Ok(match self.call_tool(name, args).await {
                    Ok(value) => json!({
                        "content": [{ "type": "text", "text": value.to_string() }],
                        "isError": false,
                    }),
                    Err(err) => json!({
                        "content": [{ "type": "text", "text": err.to_string() }],
                        "isError": true,
                    }),
                })
            }
            other => Err((
                METHOD_NOT_FOUND,
                format!("method `{other}` is not supported"),
            )),
        }
    }

    /// Handles one JSON-RPC message. Notifications (and responses) yield `None`.
    pub async fn handle(&self, message: &Value) -> Option<Value> {
        let error = |id: Value, code: i64, text: String| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": text } });
        let Some(object) = message.as_object() else {
            return Some(error(
                Value::Null,
                INVALID_REQUEST,
                "expected an object".into(),
            ));
        };
        let id = object.get("id").cloned();
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            // A response to something we never asked, or malformed: only complain about the latter.
            return match (
                id,
                object.contains_key("result") || object.contains_key("error"),
            ) {
                (_, true) => None,
                (id, false) => Some(error(
                    id.unwrap_or(Value::Null),
                    INVALID_REQUEST,
                    "missing method".into(),
                )),
            };
        };
        let id = id?;
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        Some(match self.dispatch(method, &params).await {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, text)) => error(id, code, text),
        })
    }

    /// Handles one line of input; `None` when nothing is to be written back.
    pub async fn handle_line(&self, line: &str) -> Option<String> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(err) => {
                return Some(
                    json!({ "jsonrpc": "2.0", "id": null, "error": { "code": PARSE_ERROR, "message": err.to_string() } })
                        .to_string(),
                );
            }
        };
        if let Value::Array(batch) = &message {
            let mut responses = Vec::new();
            for item in batch {
                if let Some(response) = self.handle(item).await {
                    responses.push(response);
                }
            }
            return (!responses.is_empty()).then(|| Value::Array(responses).to_string());
        }
        self.handle(&message)
            .await
            .map(|response| response.to_string())
    }

    /// Serves until the reader reaches end of input.
    pub async fn serve<R, W>(&self, reader: R, mut writer: W) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(response) = self.handle_line(&line).await {
                writer.write_all(response.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
        }
        Ok(())
    }

    /// Serves on the process's stdin and stdout.
    pub async fn serve_stdio(&self) -> Result<()> {
        self.serve(
            tokio::io::BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
        )
        .await
    }
}
