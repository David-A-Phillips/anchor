//! The SQLite system of record: units, the materialised graph, the linker and full-text search.
//!
//! Units are stored whole (canonical JSON, zstd-compressed) and decomposed into *claims*: node
//! claims, edge claims and chunks. Nodes and edges are materialised from the claims and every
//! mutation reports the resulting [`GraphDelta`].
//!
//! # Identity
//!
//! A node is identified by its keys. For kinds where [`NodeKind::merges_by_alias`] holds, a claim
//! with an unknown key attaches to the single existing node of the same kind that shares one of
//! its identity aliases (key body or alias), so `person:asmith` and `person:alice-smith` become
//! one node once a source lists both names. Limitation: two nodes that already exist are not
//! merged retroactively and a node is not split when a bridging claim disappears; [`GraphStore::rebuild`]
//! re-materialises everything in a canonical order (most trusted sources first) and heals both.
//!
//! # Linker
//!
//! Edge claims whose endpoints do not resolve stay pending and are re-evaluated whenever a node
//! or alias with a matching name appears or disappears (at most [`RELINK_LIMIT`] claims per name
//! and operation, which bounds the work a very common name can cause).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::base::{
    Attrs, ChunkId, ChunkKind, ContentHash, Db, EdgeAssertion, EdgeId, EdgeKind, Error, GraphDelta,
    GraphEdge, GraphNode, Hlc, Lookup, Migration, NodeId, NodeKey, NodeKind, NodeRef, Result,
    SourceDescriptor, SourceId, SourceKind, TrustLevel, Unit, UnitKey, UnitKind, UnitMeta,
    UnitVersion, attrs, keys,
};

/// Maximum number of edge claims re-evaluated per changed name in one operation.
pub const RELINK_LIMIT: usize = 5_000;

/// Schema history of the graph store tables.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "graph store",
    sql: "
CREATE TABLE sources (
    id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL, trust INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE units (
    id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE, source TEXT NOT NULL, kind TEXT NOT NULL,
    hash BLOB NOT NULL, hlc INTEGER NOT NULL, origin TEXT NOT NULL, deleted INTEGER NOT NULL,
    body BLOB
);
CREATE INDEX units_source ON units(source);
CREATE TABLE nodes (
    id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE, kind INTEGER NOT NULL, name TEXT NOT NULL,
    attrs TEXT NOT NULL, trust INTEGER NOT NULL, source TEXT NOT NULL, path TEXT
);
CREATE INDEX nodes_kind ON nodes(kind);
CREATE TABLE node_keys (key TEXT PRIMARY KEY, node_id INTEGER NOT NULL) WITHOUT ROWID;
CREATE INDEX node_keys_node ON node_keys(node_id);
CREATE TABLE node_claims (
    unit_id INTEGER NOT NULL, key TEXT NOT NULL, node_id INTEGER NOT NULL, name TEXT NOT NULL,
    aliases TEXT NOT NULL, attrs TEXT NOT NULL, PRIMARY KEY (unit_id, key)
) WITHOUT ROWID;
CREATE INDEX node_claims_node ON node_claims(node_id);
CREATE TABLE aliases (
    kind INTEGER NOT NULL, alias TEXT NOT NULL, node_id INTEGER NOT NULL,
    PRIMARY KEY (kind, alias, node_id)
) WITHOUT ROWID;
CREATE INDEX aliases_node ON aliases(node_id);
CREATE TABLE edges (
    id INTEGER PRIMARY KEY, from_id INTEGER NOT NULL, to_id INTEGER NOT NULL, kind INTEGER NOT NULL,
    trust INTEGER NOT NULL, weight REAL NOT NULL, UNIQUE (from_id, to_id, kind)
);
CREATE INDEX edges_to ON edges(to_id);
CREATE TABLE edge_claims (
    id INTEGER PRIMARY KEY, unit_id INTEGER NOT NULL, from_ref TEXT NOT NULL, to_ref TEXT NOT NULL,
    from_text TEXT NOT NULL, to_text TEXT NOT NULL, kind INTEGER NOT NULL, weight REAL NOT NULL,
    attrs TEXT NOT NULL, edge_id INTEGER
);
CREATE INDEX edge_claims_unit ON edge_claims(unit_id);
CREATE INDEX edge_claims_edge ON edge_claims(edge_id);
CREATE INDEX edge_claims_from ON edge_claims(from_text);
CREATE INDEX edge_claims_to ON edge_claims(to_text);
CREATE TABLE chunks (
    id INTEGER PRIMARY KEY, unit_id INTEGER NOT NULL, node_id INTEGER NOT NULL, node_key TEXT NOT NULL,
    ordinal INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL, hash BLOB NOT NULL
);
CREATE INDEX chunks_unit ON chunks(unit_id);
CREATE INDEX chunks_node ON chunks(node_id);
CREATE VIRTUAL TABLE chunks_fts USING fts5(text, content='chunks', content_rowid='id');
CREATE VIRTUAL TABLE nodes_fts USING fts5(name, content='nodes', content_rowid='id');
",
}];

/// What a write did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ApplyOutcome {
    /// Change to the materialised graph.
    pub delta: GraphDelta,
    /// `false` when the operation was a no-op (identical content or stale remote version).
    pub changed: bool,
    /// Chunks created (to be embedded).
    pub chunks_upserted: Vec<ChunkId>,
    /// Chunks removed (to be dropped from the vector index).
    pub chunks_removed: Vec<ChunkId>,
}

impl ApplyOutcome {
    fn absorb(&mut self, other: ApplyOutcome) {
        self.delta.extend(other.delta);
        self.changed |= other.changed;
        self.chunks_upserted.extend(other.chunks_upserted);
        self.chunks_removed.extend(other.chunks_removed);
    }
}

/// A materialised node with everything known about it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    /// Row id.
    pub id: NodeId,
    /// Primary key.
    pub key: NodeKey,
    /// Every key that resolves to this node.
    pub keys: Vec<NodeKey>,
    /// Kind.
    pub kind: NodeKind,
    /// Display name (from the most authoritative claim).
    pub name: String,
    /// Normalised aliases.
    pub aliases: Vec<String>,
    /// Merged attributes.
    pub attrs: Attrs,
    /// Greatest trust among asserting sources.
    pub trust: TrustLevel,
    /// Sources asserting the node.
    pub sources: Vec<SourceId>,
}

/// One unit's claim that an edge holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EdgeClaim {
    /// Unit making the claim.
    pub unit: UnitKey,
    /// Its source.
    pub source: SourceId,
    /// Trust of that source.
    pub trust: TrustLevel,
    /// Claimed confidence.
    pub weight: f32,
    /// Claim attributes (e.g. `reason`).
    pub attrs: Attrs,
}

/// A materialised edge with its provenance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EdgeRecord {
    /// Row id.
    pub id: EdgeId,
    /// Tail.
    pub from: NodeId,
    /// Head.
    pub to: NodeId,
    /// Relationship.
    pub kind: EdgeKind,
    /// Greatest trust among claims.
    pub trust: TrustLevel,
    /// Greatest weight among claims.
    pub weight: f32,
    /// Claims, most trusted first.
    pub claims: Vec<EdgeClaim>,
}

/// A stored text chunk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkRecord {
    /// Row id.
    pub id: ChunkId,
    /// Node the text belongs to.
    pub node: NodeId,
    /// Kind of text.
    pub kind: ChunkKind,
    /// Position among the node's chunks in its unit.
    pub ordinal: u32,
    /// The text.
    pub text: String,
    /// Unit that asserted it.
    pub unit: UnitKey,
    /// Source of that unit.
    pub source: SourceId,
    /// Trust of that source.
    pub trust: TrustLevel,
    /// BLAKE3 of the text.
    pub hash: ContentHash,
}

/// Result of resolving a [`Lookup`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Exactly one best candidate.
    Resolved(NodeId),
    /// Several equally good candidates.
    Ambiguous(Vec<NodeId>),
    /// No candidate.
    NotFound,
}

/// A full-text hit on a chunk.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChunkHit {
    /// The chunk.
    pub chunk: ChunkId,
    /// Its node.
    pub node: NodeId,
    /// Relevance, greater is better.
    pub score: f32,
}

/// A name/alias hit on a node.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NodeHit {
    /// The node.
    pub node: NodeId,
    /// Relevance: 1.0 exact alias, 0.7 alias prefix, below that full-text on names.
    pub score: f32,
}

/// An edge assertion that does not currently resolve.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PendingEdge {
    /// Unit that asserted it.
    pub unit: UnitKey,
    /// Relationship.
    pub kind: EdgeKind,
    /// Tail reference.
    pub from: NodeRef,
    /// Head reference.
    pub to: NodeRef,
}

/// Counts for dashboards and reports.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StoreStats {
    /// Nodes per kind.
    pub nodes: BTreeMap<String, u64>,
    /// Total nodes.
    pub node_count: u64,
    /// Materialised edges.
    pub edge_count: u64,
    /// Live units.
    pub unit_count: u64,
    /// Chunks.
    pub chunk_count: u64,
    /// Pending edge claims.
    pub pending_edges: u64,
    /// Bytes on disk.
    pub db_bytes: u64,
}

/// The graph store. Cheap to clone.
#[derive(Clone, Debug)]
pub struct GraphStore {
    db: Db,
}

fn json<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn trust_of(code: i64) -> TrustLevel {
    TrustLevel::from_code(code as u8).unwrap_or(TrustLevel::ReferenceOnly)
}

fn ref_text(reference: &NodeRef) -> String {
    match reference {
        NodeRef::Key(key) => key.as_str().to_owned(),
        NodeRef::Lookup(lookup) => lookup.text.trim().to_owned(),
    }
}

fn key_body(key: &NodeKey) -> &str {
    key.as_str().split_once(':').map_or("", |(_, body)| body)
}

fn fts_query(text: &str) -> String {
    text.split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Tracks which rows an operation touched so the delta can be computed from before/after state.
#[derive(Default)]
struct Touched {
    nodes: HashMap<i64, Option<GraphNode>>,
    edges: HashMap<i64, Option<GraphEdge>>,
    names: BTreeSet<String>,
    chunks_upserted: Vec<ChunkId>,
    chunks_removed: Vec<ChunkId>,
}

fn read_graph_node(tx: &Connection, id: i64) -> Result<Option<GraphNode>> {
    Ok(tx
        .prepare_cached("SELECT kind, trust FROM nodes WHERE id = ?1")?
        .query_row([id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .optional()?
        .and_then(|(kind, trust)| {
            Some(GraphNode {
                id: NodeId(id),
                kind: NodeKind::from_code(kind as u8)?,
                trust: trust_of(trust),
            })
        }))
}

fn read_graph_edge(tx: &Connection, id: i64) -> Result<Option<GraphEdge>> {
    Ok(tx
        .prepare_cached("SELECT from_id, to_id, kind, trust, weight FROM edges WHERE id = ?1")?
        .query_row([id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, f64>(4)?,
            ))
        })
        .optional()?
        .and_then(|(from, to, kind, trust, weight)| {
            Some(GraphEdge {
                id: EdgeId(id),
                from: NodeId(from),
                to: NodeId(to),
                kind: EdgeKind::from_code(kind as u8)?,
                trust: trust_of(trust),
                weight: weight as f32,
            })
        }))
}

impl Touched {
    fn node(&mut self, tx: &Connection, id: i64) -> Result<()> {
        if !self.nodes.contains_key(&id) {
            let before = read_graph_node(tx, id)?;
            self.nodes.insert(id, before);
        }
        Ok(())
    }

    fn edge(&mut self, tx: &Connection, id: i64) -> Result<()> {
        if !self.edges.contains_key(&id) {
            let before = read_graph_edge(tx, id)?;
            self.edges.insert(id, before);
        }
        Ok(())
    }

    fn finish(self, tx: &Connection, changed: bool) -> Result<ApplyOutcome> {
        let mut delta = GraphDelta::default();
        let mut node_ids: Vec<_> = self.nodes.into_iter().collect();
        node_ids.sort_by_key(|(id, _)| *id);
        for (id, before) in node_ids {
            match (before, read_graph_node(tx, id)?) {
                (Some(_), None) => delta.nodes_removed.push(NodeId(id)),
                (before, Some(after)) if before != Some(after) => delta.nodes_upserted.push(after),
                _ => {}
            }
        }
        let mut edge_ids: Vec<_> = self.edges.into_iter().collect();
        edge_ids.sort_by_key(|(id, _)| *id);
        for (id, before) in edge_ids {
            match (before, read_graph_edge(tx, id)?) {
                (Some(_), None) => delta.edges_removed.push(EdgeId(id)),
                (before, Some(after)) if before != Some(after) => delta.edges_upserted.push(after),
                _ => {}
            }
        }
        Ok(ApplyOutcome {
            delta,
            changed,
            chunks_upserted: self.chunks_upserted,
            chunks_removed: self.chunks_removed,
        })
    }
}

fn source_trust(tx: &Connection, source: &SourceId) -> Result<TrustLevel> {
    tx.prepare_cached("SELECT trust FROM sources WHERE id = ?1")?
        .query_row([source.as_str()], |row| row.get::<_, i64>(0))
        .optional()?
        .map(trust_of)
        .ok_or_else(|| Error::not_found(format!("source `{source}` is not registered")))
}

fn node_by_key(tx: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(tx
        .prepare_cached("SELECT node_id FROM node_keys WHERE key = ?1")?
        .query_row([key], |row| row.get(0))
        .optional()?)
}

fn alias_candidates(tx: &Connection, kind: NodeKind, alias: &str) -> Result<Vec<i64>> {
    let mut statement = tx.prepare_cached(
        "SELECT node_id FROM aliases WHERE kind = ?1 AND alias = ?2 ORDER BY node_id",
    )?;
    let rows = statement.query_map(params![kind.code(), alias], |row| row.get(0))?;
    Ok(rows.collect::<std::result::Result<Vec<i64>, _>>()?)
}

fn scope_rank(tx: &Connection, node: i64, lookup: &Lookup) -> Result<u32> {
    let Some(scope) = &lookup.scope else {
        return Ok(0);
    };
    let (source, path): (String, Option<String>) = tx
        .prepare_cached("SELECT source, path FROM nodes WHERE id = ?1")?
        .query_row([node], |row| Ok((row.get(0)?, row.get(1)?)))?;
    if scope.source.as_ref().map(SourceId::as_str) != Some(source.as_str()) {
        return Ok(0);
    }
    let (Some(wanted), Some(path)) = (&scope.path, &path) else {
        return Ok(1);
    };
    if wanted == path {
        return Ok(1_000_000);
    }
    let shared = wanted
        .split('/')
        .zip(path.split('/'))
        .take_while(|(a, b)| a == b)
        .count() as u32;
    Ok(2 + shared)
}

fn resolve_lookup(tx: &Connection, lookup: &Lookup) -> Result<Resolution> {
    for kind in &lookup.kinds {
        let alias = keys::normalize_alias(*kind, &lookup.text);
        if alias.is_empty() {
            continue;
        }
        let candidates = alias_candidates(tx, *kind, &alias)?;
        match candidates.len() {
            0 => continue,
            1 => return Ok(Resolution::Resolved(NodeId(candidates[0]))),
            _ => {
                let mut ranked = Vec::with_capacity(candidates.len());
                for candidate in &candidates {
                    ranked.push((scope_rank(tx, *candidate, lookup)?, *candidate));
                }
                let best = ranked.iter().map(|(rank, _)| *rank).max().unwrap_or(0);
                let winners: Vec<i64> = ranked
                    .iter()
                    .filter(|(rank, _)| *rank == best)
                    .map(|(_, id)| *id)
                    .collect();
                return Ok(if winners.len() == 1 {
                    Resolution::Resolved(NodeId(winners[0]))
                } else {
                    Resolution::Ambiguous(candidates.into_iter().map(NodeId).collect())
                });
            }
        }
    }
    Ok(Resolution::NotFound)
}

fn resolve_ref(tx: &Connection, reference: &NodeRef) -> Result<Option<i64>> {
    match reference {
        NodeRef::Key(key) => node_by_key(tx, key.as_str()),
        NodeRef::Lookup(lookup) => Ok(match resolve_lookup(tx, lookup)? {
            Resolution::Resolved(id) => Some(id.0),
            _ => None,
        }),
    }
}

/// Recomputes a node from its claims; deletes it (and its edges) when no claim is left.
fn refresh_node(tx: &Connection, touched: &mut Touched, node: i64) -> Result<()> {
    touched.node(tx, node)?;
    let Some(kind_code) = tx
        .prepare_cached("SELECT kind FROM nodes WHERE id = ?1")?
        .query_row([node], |row| row.get::<_, i64>(0))
        .optional()?
    else {
        return Ok(());
    };
    let kind = NodeKind::from_code(kind_code as u8).unwrap_or(NodeKind::File);
    let mut old_aliases = Vec::new();
    {
        let mut statement = tx.prepare_cached("SELECT alias FROM aliases WHERE node_id = ?1")?;
        for alias in statement.query_map([node], |row| row.get::<_, String>(0))? {
            old_aliases.push(alias?);
        }
    }
    struct Claim {
        key: String,
        name: String,
        aliases: Vec<String>,
        attrs: Attrs,
        trust: i64,
        source: String,
    }
    let mut claims = Vec::new();
    {
        let mut statement = tx.prepare_cached(
            "SELECT c.key, c.name, c.aliases, c.attrs, s.trust, u.source
             FROM node_claims c JOIN units u ON u.id = c.unit_id JOIN sources s ON s.id = u.source
             WHERE c.node_id = ?1 ORDER BY s.trust DESC, u.hlc DESC, u.key ASC",
        )?;
        let rows = statement.query_map([node], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        for row in rows {
            let (key, name, aliases, attrs, trust, source) = row?;
            claims.push(Claim {
                key,
                name,
                aliases: serde_json::from_str(&aliases)?,
                attrs: serde_json::from_str(&attrs)?,
                trust,
                source,
            });
        }
    }
    tx.prepare_cached("DELETE FROM aliases WHERE node_id = ?1")?
        .execute([node])?;
    tx.prepare_cached("DELETE FROM node_keys WHERE node_id = ?1")?
        .execute([node])?;
    touched.names.extend(old_aliases);

    if claims.is_empty() {
        let mut edges = Vec::new();
        {
            let mut statement =
                tx.prepare_cached("SELECT id FROM edges WHERE from_id = ?1 OR to_id = ?1")?;
            for id in statement.query_map([node], |row| row.get::<_, i64>(0))? {
                edges.push(id?);
            }
        }
        for edge in edges {
            touched.edge(tx, edge)?;
            tx.prepare_cached("UPDATE edge_claims SET edge_id = NULL WHERE edge_id = ?1")?
                .execute([edge])?;
            tx.prepare_cached("DELETE FROM edges WHERE id = ?1")?
                .execute([edge])?;
        }
        let name: String = tx
            .prepare_cached("SELECT name FROM nodes WHERE id = ?1")?
            .query_row([node], |row| row.get(0))?;
        tx.prepare_cached(
            "INSERT INTO nodes_fts(nodes_fts, rowid, name) VALUES ('delete', ?1, ?2)",
        )?
        .execute(params![node, name])?;
        tx.prepare_cached("DELETE FROM nodes WHERE id = ?1")?
            .execute([node])?;
        return Ok(());
    }

    let mut attrs = Attrs::new();
    let mut aliases: BTreeSet<String> = BTreeSet::new();
    let mut all_keys: BTreeSet<String> = BTreeSet::new();
    for claim in &claims {
        for (name, value) in &claim.attrs {
            attrs.entry(name.clone()).or_insert_with(|| value.clone());
        }
        all_keys.insert(claim.key.clone());
        let body = keys::normalize_alias(kind, key_body(&NodeKey::new(claim.key.clone())));
        if kind.merges_by_alias() && !body.is_empty() {
            aliases.insert(body);
        }
        for alias in claim.aliases.iter().chain(std::iter::once(&claim.name)) {
            let alias = keys::normalize_alias(kind, alias);
            if !alias.is_empty() {
                aliases.insert(alias);
            }
        }
    }
    let first = &claims[0];
    let primary = all_keys.iter().next().cloned().unwrap_or_default();
    let path = attrs
        .get(attrs::PATH)
        .and_then(Value::as_str)
        .map(str::to_owned);
    let old_name: String = tx
        .prepare_cached("SELECT name FROM nodes WHERE id = ?1")?
        .query_row([node], |row| row.get(0))?;
    tx.prepare_cached("INSERT INTO nodes_fts(nodes_fts, rowid, name) VALUES ('delete', ?1, ?2)")?
        .execute(params![node, old_name])?;
    tx.prepare_cached(
        "UPDATE nodes SET key = ?2, name = ?3, attrs = ?4, trust = ?5, source = ?6, path = ?7 WHERE id = ?1",
    )?
    .execute(params![node, primary, first.name, json(&attrs)?, first.trust, first.source, path])?;
    tx.prepare_cached("INSERT INTO nodes_fts(rowid, name) VALUES (?1, ?2)")?
        .execute(params![node, first.name])?;
    for key in &all_keys {
        tx.prepare_cached("INSERT OR REPLACE INTO node_keys (key, node_id) VALUES (?1, ?2)")?
            .execute(params![key, node])?;
        touched.names.insert(key.clone());
    }
    for alias in &aliases {
        tx.prepare_cached(
            "INSERT OR IGNORE INTO aliases (kind, alias, node_id) VALUES (?1, ?2, ?3)",
        )?
        .execute(params![kind.code(), alias, node])?;
    }
    touched.names.extend(aliases);
    Ok(())
}

/// Recomputes an edge from its claims; deletes it when no claim is left.
fn refresh_edge(tx: &Connection, touched: &mut Touched, edge: i64) -> Result<()> {
    touched.edge(tx, edge)?;
    let aggregate: Option<(Option<i64>, Option<f64>)> = tx
        .prepare_cached(
            "SELECT MAX(s.trust), MAX(c.weight) FROM edge_claims c
             JOIN units u ON u.id = c.unit_id JOIN sources s ON s.id = u.source WHERE c.edge_id = ?1",
        )?
        .query_row([edge], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    match aggregate {
        Some((Some(trust), Some(weight))) => {
            tx.prepare_cached("UPDATE edges SET trust = ?2, weight = ?3 WHERE id = ?1")?
                .execute(params![edge, trust, weight])?;
        }
        _ => {
            tx.prepare_cached("DELETE FROM edges WHERE id = ?1")?
                .execute([edge])?;
        }
    }
    Ok(())
}

/// Resolves one edge claim and (re)attaches it to its edge. Returns the edges whose aggregates
/// must be refreshed.
fn link_claim(tx: &Connection, touched: &mut Touched, claim: i64) -> Result<()> {
    let (from_ref, to_ref, kind, current): (String, String, i64, Option<i64>) = tx
        .prepare_cached("SELECT from_ref, to_ref, kind, edge_id FROM edge_claims WHERE id = ?1")?
        .query_row([claim], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
    let from: NodeRef = serde_json::from_str(&from_ref)?;
    let to: NodeRef = serde_json::from_str(&to_ref)?;
    let target = match (resolve_ref(tx, &from)?, resolve_ref(tx, &to)?) {
        (Some(from), Some(to)) if from != to => {
            let existing: Option<i64> = tx
                .prepare_cached(
                    "SELECT id FROM edges WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3",
                )?
                .query_row(params![from, to, kind], |row| row.get(0))
                .optional()?;
            Some(match existing {
                Some(id) => id,
                None => {
                    tx.prepare_cached(
                        "INSERT INTO edges (from_id, to_id, kind, trust, weight) VALUES (?1, ?2, ?3, 0, 1.0)",
                    )?
                    .execute(params![from, to, kind])?;
                    let id = tx.last_insert_rowid();
                    touched.edges.entry(id).or_insert(None);
                    id
                }
            })
        }
        _ => None,
    };
    if target != current {
        tx.prepare_cached("UPDATE edge_claims SET edge_id = ?2 WHERE id = ?1")?
            .execute(params![claim, target])?;
        if let Some(old) = current {
            refresh_edge(tx, touched, old)?;
        }
    }
    if let Some(new) = target {
        refresh_edge(tx, touched, new)?;
    }
    Ok(())
}

/// Re-evaluates the edge claims that refer to any name that changed during this operation.
fn relink(tx: &Connection, touched: &mut Touched) -> Result<()> {
    let names: Vec<String> = std::mem::take(&mut touched.names).into_iter().collect();
    let mut claims = BTreeSet::new();
    for name in &names {
        // Lookups store the text as written; match its normalised forms too.
        let mut statement = tx.prepare_cached(
            "SELECT id FROM edge_claims WHERE from_text = ?1 OR to_text = ?1
             OR from_text = ?2 OR to_text = ?2 LIMIT ?3",
        )?;
        let slug = keys::slugify(name);
        for id in statement.query_map(params![name, slug, RELINK_LIMIT as i64], |row| {
            row.get::<_, i64>(0)
        })? {
            claims.insert(id?);
        }
    }
    for claim in claims {
        link_claim(tx, touched, claim)?;
    }
    Ok(())
}

fn remove_unit_rows(tx: &Connection, touched: &mut Touched, unit: i64) -> Result<BTreeSet<i64>> {
    let mut nodes = BTreeSet::new();
    {
        let mut statement =
            tx.prepare_cached("SELECT node_id FROM node_claims WHERE unit_id = ?1")?;
        for id in statement.query_map([unit], |row| row.get::<_, i64>(0))? {
            nodes.insert(id?);
        }
    }
    tx.prepare_cached("DELETE FROM node_claims WHERE unit_id = ?1")?
        .execute([unit])?;
    let mut edges = BTreeSet::new();
    {
        let mut statement = tx.prepare_cached(
            "SELECT edge_id FROM edge_claims WHERE unit_id = ?1 AND edge_id IS NOT NULL",
        )?;
        for id in statement.query_map([unit], |row| row.get::<_, i64>(0))? {
            edges.insert(id?);
        }
    }
    tx.prepare_cached("DELETE FROM edge_claims WHERE unit_id = ?1")?
        .execute([unit])?;
    for edge in edges {
        refresh_edge(tx, touched, edge)?;
    }
    Ok(nodes)
}

struct OldChunk {
    id: i64,
    text: String,
}

fn write_unit(
    tx: &Connection,
    touched: &mut Touched,
    unit: &Unit,
    hash: ContentHash,
    version: &UnitVersion,
) -> Result<bool> {
    source_trust(tx, &unit.source)?;
    let existing: Option<(i64, Vec<u8>, i64)> = tx
        .prepare_cached("SELECT id, hash, deleted FROM units WHERE key = ?1")?
        .query_row([unit.key.as_str()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()?;
    if let Some((_, old_hash, deleted)) = &existing
        && *deleted == 0
        && old_hash.as_slice() == hash.as_bytes()
    {
        return Ok(false);
    }
    let body = zstd::encode_all(crate::base::canonical_json(unit)?.as_slice(), 3)?;
    let unit_id = match existing {
        Some((id, _, _)) => {
            tx.prepare_cached(
                "UPDATE units SET source = ?2, kind = ?3, hash = ?4, hlc = ?5, origin = ?6, deleted = 0, body = ?7 WHERE id = ?1",
            )?
            .execute(params![
                id,
                unit.source.as_str(),
                unit.kind.as_str(),
                hash.as_bytes().as_slice(),
                version.hlc.as_i64(),
                version.origin.as_str(),
                body
            ])?;
            id
        }
        None => {
            tx.prepare_cached(
                "INSERT INTO units (key, source, kind, hash, hlc, origin, deleted, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
            )?
            .execute(params![
                unit.key.as_str(),
                unit.source.as_str(),
                unit.kind.as_str(),
                hash.as_bytes().as_slice(),
                version.hlc.as_i64(),
                version.origin.as_str(),
                body
            ])?;
            tx.last_insert_rowid()
        }
    };

    let mut affected = remove_unit_rows(tx, touched, unit_id)?;

    // Node claims.
    for node in &unit.nodes {
        let node_id = match node_by_key(tx, node.key.as_str())? {
            Some(id) => id,
            None => {
                let mut attach = None;
                if node.kind.merges_by_alias() {
                    let mut candidates = BTreeSet::new();
                    for alias in node
                        .aliases
                        .iter()
                        .map(String::as_str)
                        .chain([key_body(&node.key)])
                    {
                        let alias = keys::normalize_alias(node.kind, alias);
                        if !alias.is_empty() {
                            candidates.extend(alias_candidates(tx, node.kind, &alias)?);
                        }
                    }
                    if candidates.len() == 1 {
                        attach = candidates.into_iter().next();
                    }
                }
                match attach {
                    Some(id) => id,
                    None => {
                        tx.prepare_cached(
                            "INSERT INTO nodes (key, kind, name, attrs, trust, source, path) VALUES (?1, ?2, ?3, '{}', 0, ?4, NULL)",
                        )?
                        .execute(params![node.key.as_str(), node.kind.code(), node.name, unit.source.as_str()])?;
                        let id = tx.last_insert_rowid();
                        tx.prepare_cached("INSERT INTO nodes_fts(rowid, name) VALUES (?1, ?2)")?
                            .execute(params![id, node.name])?;
                        touched.nodes.entry(id).or_insert(None);
                        id
                    }
                }
            }
        };
        tx.prepare_cached("INSERT OR REPLACE INTO node_keys (key, node_id) VALUES (?1, ?2)")?
            .execute(params![node.key.as_str(), node_id])?;
        tx.prepare_cached(
            "INSERT OR REPLACE INTO node_claims (unit_id, key, node_id, name, aliases, attrs) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?
        .execute(params![unit_id, node.key.as_str(), node_id, node.name, json(&node.aliases)?, json(&node.attrs)?])?;
        affected.insert(node_id);
    }
    for node in &affected {
        refresh_node(tx, touched, *node)?;
    }

    // Edge claims.
    let mut new_claims = Vec::with_capacity(unit.edges.len());
    for edge in &unit.edges {
        insert_edge_claim(tx, unit_id, edge)?;
        new_claims.push(tx.last_insert_rowid());
    }
    for claim in new_claims {
        link_claim(tx, touched, claim)?;
    }
    relink(tx, touched)?;

    // Chunks: keep the ids of unchanged chunks.
    let mut old: HashMap<(String, u32, String), OldChunk> = HashMap::new();
    {
        let mut statement = tx.prepare_cached(
            "SELECT id, node_key, ordinal, kind, text FROM chunks WHERE unit_id = ?1",
        )?;
        let rows = statement.query_map([unit_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (id, node_key, ordinal, kind, text) = row?;
            old.insert((node_key, ordinal, kind), OldChunk { id, text });
        }
    }
    for chunk in &unit.chunks {
        let Some(node_id) = node_by_key(tx, chunk.node.as_str())? else {
            continue;
        };
        let slot = (
            chunk.node.as_str().to_owned(),
            chunk.ordinal,
            chunk.kind.as_str().to_owned(),
        );
        if let Some(previous) = old.remove(&slot) {
            if previous.text == chunk.text {
                tx.prepare_cached("UPDATE chunks SET node_id = ?2 WHERE id = ?1")?
                    .execute(params![previous.id, node_id])?;
                continue;
            }
            delete_chunk(tx, touched, previous.id, &previous.text)?;
        }
        tx.prepare_cached(
            "INSERT INTO chunks (unit_id, node_id, node_key, ordinal, kind, text, hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?
        .execute(params![
            unit_id,
            node_id,
            chunk.node.as_str(),
            chunk.ordinal,
            chunk.kind.as_str(),
            chunk.text,
            ContentHash::of(chunk.text.as_bytes()).as_bytes().as_slice()
        ])?;
        let id = tx.last_insert_rowid();
        tx.prepare_cached("INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)")?
            .execute(params![id, chunk.text])?;
        touched.chunks_upserted.push(ChunkId(id));
    }
    for (_, stale) in old {
        delete_chunk(tx, touched, stale.id, &stale.text)?;
    }
    Ok(true)
}

fn insert_edge_claim(tx: &Connection, unit_id: i64, edge: &EdgeAssertion) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO edge_claims (unit_id, from_ref, to_ref, from_text, to_text, kind, weight, attrs, edge_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
    )?
    .execute(params![
        unit_id,
        json(&edge.from)?,
        json(&edge.to)?,
        normalized_text(&edge.from),
        normalized_text(&edge.to),
        edge.kind.code(),
        f64::from(edge.weight),
        json(&edge.attrs)?
    ])?;
    Ok(())
}

/// The text a claim endpoint is indexed under: the key itself, or the lookup text normalised for
/// its first kind (so alias changes find it).
fn normalized_text(reference: &NodeRef) -> String {
    match reference {
        NodeRef::Key(key) => key.as_str().to_owned(),
        NodeRef::Lookup(lookup) => lookup
            .kinds
            .first()
            .map(|kind| keys::normalize_alias(*kind, &lookup.text))
            .unwrap_or_else(|| ref_text(reference)),
    }
}

fn delete_chunk(tx: &Connection, touched: &mut Touched, id: i64, text: &str) -> Result<()> {
    tx.prepare_cached("INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', ?1, ?2)")?
        .execute(params![id, text])?;
    tx.prepare_cached("DELETE FROM chunks WHERE id = ?1")?
        .execute([id])?;
    touched.chunks_removed.push(ChunkId(id));
    Ok(())
}

fn tombstone_unit(
    tx: &Connection,
    touched: &mut Touched,
    key: &UnitKey,
    meta: Option<&UnitMeta>,
    version: &UnitVersion,
) -> Result<bool> {
    let existing: Option<(i64, i64)> = tx
        .prepare_cached("SELECT id, deleted FROM units WHERE key = ?1")?
        .query_row([key.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let unit_id = match existing {
        Some((id, _)) => id,
        None => {
            let Some(meta) = meta else {
                return Ok(false);
            };
            tx.prepare_cached(
                "INSERT INTO units (key, source, kind, hash, hlc, origin, deleted, body) VALUES (?1, ?2, ?3, ?4, 0, '', 1, NULL)",
            )?
            .execute(params![
                key.as_str(),
                meta.source.as_str(),
                meta.kind.as_str(),
                ContentHash::ZERO.as_bytes().as_slice()
            ])?;
            tx.last_insert_rowid()
        }
    };
    let nodes = remove_unit_rows(tx, touched, unit_id)?;
    let mut chunks = Vec::new();
    {
        let mut statement = tx.prepare_cached("SELECT id, text FROM chunks WHERE unit_id = ?1")?;
        for row in statement.query_map([unit_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })? {
            chunks.push(row?);
        }
    }
    for (id, text) in chunks {
        delete_chunk(tx, touched, id, &text)?;
    }
    for node in nodes {
        refresh_node(tx, touched, node)?;
    }
    relink(tx, touched)?;
    tx.prepare_cached(
        "UPDATE units SET deleted = 1, hash = ?2, hlc = ?3, origin = ?4, body = NULL WHERE id = ?1",
    )?
    .execute(params![
        unit_id,
        ContentHash::ZERO.as_bytes().as_slice(),
        version.hlc.as_i64(),
        version.origin.as_str()
    ])?;
    Ok(true)
}

fn read_meta(tx: &Connection, key: &UnitKey) -> Result<Option<UnitMeta>> {
    let row: Option<(String, String, Vec<u8>, i64, String, i64)> = tx
        .prepare_cached(
            "SELECT source, kind, hash, hlc, origin, deleted FROM units WHERE key = ?1",
        )?
        .query_row([key.as_str()], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .optional()?;
    row.map(|(source, kind, hash, hlc, origin, deleted)| {
        Ok(UnitMeta {
            key: key.clone(),
            source: SourceId::new(source),
            kind: kind.parse::<UnitKind>()?,
            hash: ContentHash::from_slice(&hash)?,
            hlc: Hlc::from_i64(hlc),
            origin: origin.into(),
            deleted: deleted != 0,
        })
    })
    .transpose()
}

impl GraphStore {
    /// Opens the store on a database whose schema includes [`MIGRATIONS`].
    pub fn new(db: Db) -> Result<Self> {
        Ok(Self { db })
    }

    /// The underlying database handle.
    pub fn db(&self) -> &Db {
        &self.db
    }

    fn write(
        &self,
        f: impl FnOnce(&Transaction<'_>, &mut Touched) -> Result<bool>,
    ) -> Result<ApplyOutcome> {
        self.db.transaction(|tx| {
            let mut touched = Touched::default();
            let changed = f(tx, &mut touched)?;
            touched.finish(tx, changed)
        })
    }

    /// Registers a source or updates it. Changing the trust level re-weights everything the
    /// source contributes to.
    pub fn upsert_source(&self, source: &SourceDescriptor) -> Result<GraphDelta> {
        let outcome = self.write(|tx, touched| {
            let previous: Option<i64> = tx
                .prepare_cached("SELECT trust FROM sources WHERE id = ?1")?
                .query_row([source.id.as_str()], |row| row.get(0))
                .optional()?;
            tx.prepare_cached(
                "INSERT INTO sources (id, kind, name, trust) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, name = excluded.name, trust = excluded.trust",
            )?
            .execute(params![source.id.as_str(), source.kind.as_str(), source.name, source.trust.code()])?;
            if previous.is_none() || previous == Some(i64::from(source.trust.code())) {
                return Ok(previous.is_none());
            }
            let mut nodes = Vec::new();
            {
                let mut statement = tx.prepare(
                    "SELECT DISTINCT c.node_id FROM node_claims c JOIN units u ON u.id = c.unit_id WHERE u.source = ?1",
                )?;
                for id in statement.query_map([source.id.as_str()], |row| row.get::<_, i64>(0))? {
                    nodes.push(id?);
                }
            }
            let mut edges = Vec::new();
            {
                let mut statement = tx.prepare(
                    "SELECT DISTINCT c.edge_id FROM edge_claims c JOIN units u ON u.id = c.unit_id
                     WHERE u.source = ?1 AND c.edge_id IS NOT NULL",
                )?;
                for id in statement.query_map([source.id.as_str()], |row| row.get::<_, i64>(0))? {
                    edges.push(id?);
                }
            }
            for node in nodes {
                refresh_node(tx, touched, node)?;
            }
            touched.names.clear();
            for edge in edges {
                refresh_edge(tx, touched, edge)?;
            }
            Ok(true)
        })?;
        Ok(outcome.delta)
    }

    /// Every registered source.
    pub fn sources(&self) -> Result<Vec<SourceDescriptor>> {
        self.db.read(|conn| {
            let mut statement =
                conn.prepare_cached("SELECT id, kind, name, trust FROM sources ORDER BY id")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, kind, name, trust) = row?;
                out.push(SourceDescriptor {
                    id: SourceId::new(id),
                    kind: kind.parse::<SourceKind>()?,
                    name,
                    trust: trust_of(trust),
                });
            }
            Ok(out)
        })
    }

    /// Applies one unit, replacing what its previous version asserted. Identical content is a
    /// no-op and keeps the stored version stamp.
    pub fn apply_unit(&self, unit: &Unit, version: &UnitVersion) -> Result<ApplyOutcome> {
        self.apply_units(std::slice::from_ref(unit), version)
    }

    /// Applies many units in one transaction, all stamped with `version`.
    pub fn apply_units(&self, units: &[Unit], version: &UnitVersion) -> Result<ApplyOutcome> {
        self.write(|tx, touched| {
            let mut changed = false;
            for unit in units {
                let mut unit = unit.clone();
                unit.canonicalize();
                let hash = unit.content_hash()?;
                changed |= write_unit(tx, touched, &unit, hash, version)?;
            }
            Ok(changed)
        })
    }

    /// Removes a unit's assertions and leaves a tombstone. Unknown units are a no-op.
    pub fn remove_unit(&self, key: &UnitKey, version: &UnitVersion) -> Result<ApplyOutcome> {
        self.write(|tx, touched| tombstone_unit(tx, touched, key, None, version))
    }

    /// Applies a unit version received from another device, if it supersedes the local one.
    pub fn apply_remote(&self, meta: &UnitMeta, unit: Option<&Unit>) -> Result<ApplyOutcome> {
        self.write(|tx, touched| {
            if let Some(current) = read_meta(tx, &meta.key)?
                && !meta.supersedes(&current)
            {
                return Ok(false);
            }
            let version = meta.version();
            match (meta.deleted, unit) {
                (true, _) => {
                    source_trust(tx, &meta.source)?;
                    tombstone_unit(tx, touched, &meta.key, Some(meta), &version)?;
                    Ok(true)
                }
                (false, Some(unit)) => {
                    let mut unit = unit.clone();
                    unit.canonicalize();
                    let hash = unit.content_hash()?;
                    if hash != meta.hash || unit.key != meta.key {
                        return Err(Error::integrity(format!(
                            "unit `{}` does not match its declared hash",
                            meta.key
                        )));
                    }
                    if !write_unit(tx, touched, &unit, hash, &version)? {
                        tx.prepare_cached("UPDATE units SET hlc = ?2, origin = ?3 WHERE key = ?1")?
                            .execute(params![
                                meta.key.as_str(),
                                version.hlc.as_i64(),
                                version.origin.as_str()
                            ])?;
                    }
                    Ok(true)
                }
                (false, None) => Err(Error::invalid(format!(
                    "unit `{}` has no content",
                    meta.key
                ))),
            }
        })
    }

    /// Version metadata of a unit (including tombstones).
    pub fn unit_meta(&self, key: &UnitKey) -> Result<Option<UnitMeta>> {
        self.db.read(|conn| read_meta(conn, key))
    }

    /// Version metadata of every unit of a source, tombstones included, ordered by key.
    pub fn unit_metas(&self, source: &SourceId) -> Result<Vec<UnitMeta>> {
        self.db.read(|conn| {
            let mut statement =
                conn.prepare_cached("SELECT key FROM units WHERE source = ?1 ORDER BY key")?;
            let keys: Vec<String> = statement
                .query_map([source.as_str()], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                if let Some(meta) = read_meta(conn, &UnitKey::new(key))? {
                    out.push(meta);
                }
            }
            Ok(out)
        })
    }

    /// The canonical unit as stored; `None` for unknown units and tombstones.
    pub fn unit(&self, key: &UnitKey) -> Result<Option<Unit>> {
        self.db.read(|conn| {
            let body: Option<Option<Vec<u8>>> = conn
                .prepare_cached("SELECT body FROM units WHERE key = ?1 AND deleted = 0")?
                .query_row([key.as_str()], |row| row.get(0))
                .optional()?;
            match body.flatten() {
                Some(body) => Ok(Some(serde_json::from_slice(&zstd::decode_all(
                    body.as_slice(),
                )?)?)),
                None => Ok(None),
            }
        })
    }

    /// Greatest version timestamp stored; seeds the clock after a restart.
    pub fn max_hlc(&self) -> Result<Hlc> {
        self.db.read(|conn| {
            let raw: i64 =
                conn.query_row("SELECT COALESCE(MAX(hlc), 0) FROM units", [], |row| {
                    row.get(0)
                })?;
            Ok(Hlc::from_i64(raw))
        })
    }

    /// All nodes and edges, for building the in-memory graph.
    pub fn load_graph(&self) -> Result<(Vec<GraphNode>, Vec<GraphEdge>)> {
        self.db.read(|conn| {
            let mut nodes = Vec::new();
            let mut statement = conn.prepare("SELECT id, kind, trust FROM nodes ORDER BY id")?;
            for row in statement.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })? {
                let (id, kind, trust) = row?;
                if let Some(kind) = NodeKind::from_code(kind as u8) {
                    nodes.push(GraphNode {
                        id: NodeId(id),
                        kind,
                        trust: trust_of(trust),
                    });
                }
            }
            let mut edges = Vec::new();
            let mut statement = conn.prepare("SELECT id FROM edges ORDER BY id")?;
            let ids: Vec<i64> = statement
                .query_map([], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            for id in ids {
                if let Some(edge) = read_graph_edge(conn, id)? {
                    edges.push(edge);
                }
            }
            Ok((nodes, edges))
        })
    }

    fn node_record(conn: &Connection, id: i64) -> Result<Option<NodeRecord>> {
        let row: Option<(String, i64, String, String, i64)> = conn
            .prepare_cached("SELECT key, kind, name, attrs, trust FROM nodes WHERE id = ?1")?
            .query_row([id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .optional()?;
        let Some((key, kind, name, attrs, trust)) = row else {
            return Ok(None);
        };
        let Some(kind) = NodeKind::from_code(kind as u8) else {
            return Ok(None);
        };
        let strings = |sql: &str| -> Result<Vec<String>> {
            let mut statement = conn.prepare_cached(sql)?;
            let rows = statement.query_map([id], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        };
        Ok(Some(NodeRecord {
            id: NodeId(id),
            key: NodeKey::new(key),
            keys: strings("SELECT key FROM node_keys WHERE node_id = ?1 ORDER BY key")?
                .into_iter()
                .map(NodeKey::new)
                .collect(),
            kind,
            name,
            aliases: strings("SELECT alias FROM aliases WHERE node_id = ?1 ORDER BY alias")?,
            attrs: serde_json::from_str(&attrs)?,
            trust: trust_of(trust),
            sources: strings(
                "SELECT DISTINCT u.source FROM node_claims c JOIN units u ON u.id = c.unit_id WHERE c.node_id = ?1 ORDER BY u.source",
            )?
            .into_iter()
            .map(SourceId::new)
            .collect(),
        }))
    }

    /// The node with this id.
    pub fn node(&self, id: NodeId) -> Result<Option<NodeRecord>> {
        self.db.read(|conn| Self::node_record(conn, id.0))
    }

    /// The node having `key` as primary or secondary key.
    pub fn node_by_key(&self, key: &NodeKey) -> Result<Option<NodeRecord>> {
        self.db.read(|conn| match node_by_key(conn, key.as_str())? {
            Some(id) => Self::node_record(conn, id),
            None => Ok(None),
        })
    }

    /// The nodes with these ids (missing ids are skipped), in the given order.
    pub fn nodes(&self, ids: &[NodeId]) -> Result<Vec<NodeRecord>> {
        self.db.read(|conn| {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(record) = Self::node_record(conn, id.0)? {
                    out.push(record);
                }
            }
            Ok(out)
        })
    }

    /// Nodes of a kind, ordered by id.
    pub fn nodes_by_kind(
        &self,
        kind: NodeKind,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<NodeRecord>> {
        self.db.read(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT id FROM nodes WHERE kind = ?1 ORDER BY id LIMIT ?2 OFFSET ?3",
            )?;
            let ids: Vec<i64> = statement
                .query_map(params![kind.code(), limit as i64, offset as i64], |row| {
                    row.get(0)
                })?
                .collect::<std::result::Result<_, _>>()?;
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(record) = Self::node_record(conn, id)? {
                    out.push(record);
                }
            }
            Ok(out)
        })
    }

    fn edge_record(conn: &Connection, id: i64) -> Result<Option<EdgeRecord>> {
        let Some(edge) = read_graph_edge(conn, id)? else {
            return Ok(None);
        };
        let mut statement = conn.prepare_cached(
            "SELECT u.key, u.source, s.trust, c.weight, c.attrs FROM edge_claims c
             JOIN units u ON u.id = c.unit_id JOIN sources s ON s.id = u.source
             WHERE c.edge_id = ?1 ORDER BY s.trust DESC, u.key",
        )?;
        let rows = statement.query_map([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, f64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut claims = Vec::new();
        for row in rows {
            let (unit, source, trust, weight, attrs) = row?;
            claims.push(EdgeClaim {
                unit: UnitKey::new(unit),
                source: SourceId::new(source),
                trust: trust_of(trust),
                weight: weight as f32,
                attrs: serde_json::from_str(&attrs)?,
            });
        }
        Ok(Some(EdgeRecord {
            id: edge.id,
            from: edge.from,
            to: edge.to,
            kind: edge.kind,
            trust: edge.trust,
            weight: edge.weight,
            claims,
        }))
    }

    /// The edge with this id, with its claims.
    pub fn edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>> {
        self.db.read(|conn| Self::edge_record(conn, id.0))
    }

    /// Edges incident to a node, in both directions.
    pub fn edges_of(&self, node: NodeId, limit: usize) -> Result<Vec<EdgeRecord>> {
        self.db.read(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT id FROM edges WHERE from_id = ?1 OR to_id = ?1 ORDER BY id LIMIT ?2",
            )?;
            let ids: Vec<i64> = statement
                .query_map(params![node.0, limit as i64], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(record) = Self::edge_record(conn, id)? {
                    out.push(record);
                }
            }
            Ok(out)
        })
    }

    /// Resolves a lookup with the linker's rule.
    pub fn resolve(&self, lookup: &Lookup) -> Result<Resolution> {
        self.db.read(|conn| resolve_lookup(conn, lookup))
    }

    fn chunk_rows(
        conn: &Connection,
        filter: &str,
        param: i64,
        limit: i64,
    ) -> Result<Vec<ChunkRecord>> {
        let sql = format!(
            "SELECT c.id, c.node_id, c.kind, c.ordinal, c.text, u.key, u.source, s.trust, c.hash
             FROM chunks c JOIN units u ON u.id = c.unit_id JOIN sources s ON s.id = u.source
             WHERE {filter} ORDER BY c.id LIMIT ?2"
        );
        let mut statement = conn.prepare_cached(&sql)?;
        let rows = statement.query_map(params![param, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u32>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Vec<u8>>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, node, kind, ordinal, text, unit, source, trust, hash) = row?;
            out.push(ChunkRecord {
                id: ChunkId(id),
                node: NodeId(node),
                kind: kind.parse::<ChunkKind>()?,
                ordinal,
                text,
                unit: UnitKey::new(unit),
                source: SourceId::new(source),
                trust: trust_of(trust),
                hash: ContentHash::from_slice(&hash)?,
            });
        }
        Ok(out)
    }

    /// Chunks attached to a node.
    pub fn chunks_of(&self, node: NodeId) -> Result<Vec<ChunkRecord>> {
        self.db
            .read(|conn| Self::chunk_rows(conn, "c.node_id = ?1", node.0, 10_000))
    }

    /// One chunk.
    pub fn chunk(&self, id: ChunkId) -> Result<Option<ChunkRecord>> {
        self.db.read(|conn| {
            Ok(Self::chunk_rows(conn, "c.id = ?1", id.0, 1)?
                .into_iter()
                .next())
        })
    }

    /// Chunks with an id greater than `cursor`, in id order: the feed for the embedding indexer.
    pub fn chunks_after(&self, cursor: ChunkId, limit: usize) -> Result<Vec<ChunkRecord>> {
        self.db
            .read(|conn| Self::chunk_rows(conn, "c.id > ?1", cursor.0, limit as i64))
    }

    /// Full-text search over chunk text (BM25). The query is treated as plain words.
    pub fn search_chunks(&self, query: &str, limit: usize) -> Result<Vec<ChunkHit>> {
        let fts = fts_query(query);
        if fts.is_empty() {
            return Ok(Vec::new());
        }
        self.db.read(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT c.id, c.node_id, bm25(chunks_fts) FROM chunks_fts
                 JOIN chunks c ON c.id = chunks_fts.rowid WHERE chunks_fts MATCH ?1
                 ORDER BY bm25(chunks_fts), c.id LIMIT ?2",
            )?;
            let rows = statement.query_map(params![fts, limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (chunk, node, rank) = row?;
                out.push(ChunkHit {
                    chunk: ChunkId(chunk),
                    node: NodeId(node),
                    score: (-rank) as f32,
                });
            }
            Ok(out)
        })
    }

    /// Finds nodes by name: exact alias, then alias prefix, then full-text over names.
    pub fn search_nodes(
        &self,
        query: &str,
        kinds: &[NodeKind],
        limit: usize,
    ) -> Result<Vec<NodeHit>> {
        let kinds: Vec<NodeKind> = if kinds.is_empty() {
            NodeKind::ALL.to_vec()
        } else {
            kinds.to_vec()
        };
        self.db.read(|conn| {
            let mut hits: Vec<NodeHit> = Vec::new();
            let mut push = |node: i64, score: f32, hits: &mut Vec<NodeHit>| {
                if !hits.iter().any(|hit| hit.node.0 == node) {
                    hits.push(NodeHit { node: NodeId(node), score });
                }
            };
            for kind in &kinds {
                let alias = keys::normalize_alias(*kind, query);
                if alias.is_empty() {
                    continue;
                }
                for node in alias_candidates(conn, *kind, &alias)? {
                    push(node, 1.0, &mut hits);
                }
            }
            for kind in &kinds {
                let alias = keys::normalize_alias(*kind, query);
                if alias.len() < 2 || hits.len() >= limit {
                    continue;
                }
                let upper = format!("{alias}\u{10FFFF}");
                let mut statement = conn.prepare_cached(
                    "SELECT DISTINCT node_id FROM aliases WHERE kind = ?1 AND alias >= ?2 AND alias < ?3 LIMIT ?4",
                )?;
                for node in statement.query_map(params![kind.code(), alias, upper, limit as i64], |row| row.get::<_, i64>(0))? {
                    push(node?, 0.7, &mut hits);
                }
            }
            let fts = fts_query(query);
            if !fts.is_empty() && hits.len() < limit {
                let codes: Vec<i64> = kinds.iter().map(|kind| i64::from(kind.code())).collect();
                let mut statement = conn.prepare_cached(
                    "SELECT n.id, n.kind, bm25(nodes_fts) FROM nodes_fts JOIN nodes n ON n.id = nodes_fts.rowid
                     WHERE nodes_fts MATCH ?1 ORDER BY bm25(nodes_fts), n.id LIMIT ?2",
                )?;
                let rows = statement.query_map(params![fts, (limit * 4) as i64], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, f64>(2)?))
                })?;
                for row in rows {
                    let (node, kind, rank) = row?;
                    if codes.contains(&kind) {
                        push(node, (0.5 / (1.0 + rank.abs().recip())) as f32, &mut hits);
                    }
                }
            }
            hits.truncate(limit);
            Ok(hits)
        })
    }

    /// Edge assertions that do not currently resolve (gaps).
    pub fn pending_edges(&self, limit: usize) -> Result<Vec<PendingEdge>> {
        self.db.read(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT u.key, c.kind, c.from_ref, c.to_ref FROM edge_claims c JOIN units u ON u.id = c.unit_id
                 WHERE c.edge_id IS NULL ORDER BY c.id LIMIT ?1",
            )?;
            let rows = statement.query_map([limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (unit, kind, from, to) = row?;
                if let Some(kind) = EdgeKind::from_code(kind as u8) {
                    out.push(PendingEdge {
                        unit: UnitKey::new(unit),
                        kind,
                        from: serde_json::from_str(&from)?,
                        to: serde_json::from_str(&to)?,
                    });
                }
            }
            Ok(out)
        })
    }

    /// Counts.
    pub fn stats(&self) -> Result<StoreStats> {
        let mut stats = self.db.read(|conn| {
            let count = |sql: &str| -> Result<u64> {
                Ok(conn.query_row(sql, [], |row| row.get::<_, i64>(0))? as u64)
            };
            let mut stats = StoreStats {
                node_count: count("SELECT COUNT(*) FROM nodes")?,
                edge_count: count("SELECT COUNT(*) FROM edges")?,
                unit_count: count("SELECT COUNT(*) FROM units WHERE deleted = 0")?,
                chunk_count: count("SELECT COUNT(*) FROM chunks")?,
                pending_edges: count("SELECT COUNT(*) FROM edge_claims WHERE edge_id IS NULL")?,
                ..StoreStats::default()
            };
            let mut statement = conn.prepare("SELECT kind, COUNT(*) FROM nodes GROUP BY kind")?;
            for row in
                statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            {
                let (kind, n) = row?;
                if let Some(kind) = NodeKind::from_code(kind as u8) {
                    stats.nodes.insert(kind.as_str().to_owned(), n as u64);
                }
            }
            Ok(stats)
        })?;
        stats.db_bytes = self.db.size_on_disk()?;
        Ok(stats)
    }

    /// Re-materialises the whole graph from the live units, most trusted sources first. Row ids
    /// change; callers must reload the in-memory graph afterwards.
    pub fn rebuild(&self) -> Result<()> {
        let units: Vec<(Unit, UnitVersion)> = self.db.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT u.body, u.hlc, u.origin FROM units u JOIN sources s ON s.id = u.source
                 WHERE u.deleted = 0 ORDER BY s.trust DESC, u.key",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (body, hlc, origin) = row?;
                let unit: Unit = serde_json::from_slice(&zstd::decode_all(body.as_slice())?)?;
                out.push((unit, UnitVersion::new(Hlc::from_i64(hlc), origin)));
            }
            Ok(out)
        })?;
        self.db.transaction(|tx| {
            tx.execute_batch(
                "DELETE FROM nodes; DELETE FROM node_keys; DELETE FROM node_claims; DELETE FROM aliases;
                 DELETE FROM edges; DELETE FROM edge_claims; DELETE FROM chunks;
                 INSERT INTO chunks_fts(chunks_fts) VALUES ('delete-all');
                 INSERT INTO nodes_fts(nodes_fts) VALUES ('delete-all');
                 UPDATE units SET hash = zeroblob(32) WHERE deleted = 0;",
            )?;
            let mut touched = Touched::default();
            for (unit, version) in &units {
                let hash = unit.content_hash()?;
                write_unit(tx, &mut touched, unit, hash, version)?;
            }
            Ok(())
        })
    }

    /// A deterministic dump of the materialised graph keyed by natural keys, for tests:
    /// `(nodes: key -> (kind, name, trust), edges: (from key, kind, to key, trust))`.
    pub fn snapshot(
        &self,
    ) -> Result<(
        BTreeMap<String, (String, String, String)>,
        BTreeSet<(String, String, String, String)>,
    )> {
        self.db.read(|conn| {
            let mut nodes = BTreeMap::new();
            let mut statement = conn.prepare("SELECT key, kind, name, trust FROM nodes")?;
            for row in statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?))
            })? {
                let (key, kind, name, trust) = row?;
                let kind = NodeKind::from_code(kind as u8).map(NodeKind::as_str).unwrap_or("?");
                nodes.insert(key, (kind.to_owned(), name, trust_of(trust).as_str().to_owned()));
            }
            let mut edges = BTreeSet::new();
            let mut statement = conn.prepare(
                "SELECT f.key, e.kind, t.key, e.trust FROM edges e JOIN nodes f ON f.id = e.from_id JOIN nodes t ON t.id = e.to_id",
            )?;
            for row in statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?))
            })? {
                let (from, kind, to, trust) = row?;
                let kind = EdgeKind::from_code(kind as u8).map(EdgeKind::as_str).unwrap_or("?");
                edges.insert((from, kind.to_owned(), to, trust_of(trust).as_str().to_owned()));
            }
            Ok((nodes, edges))
        })
    }
}

#[cfg(test)]
mod tests;
