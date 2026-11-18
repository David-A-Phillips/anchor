//! The ingestion data model.
//!
//! Every source is ingested as a set of [`Unit`]s. A unit is the atom of ingestion, replacement,
//! content addressing and synchronisation: one source file, one wiki page with its threads, one
//! ticket, one API definition, one ownership file. A unit *asserts* nodes, edges and text chunks;
//! the graph store materialises the union of all assertions into the queryable graph and reports
//! each change as a [`GraphDelta`] for the in-memory graph.
//!
//! Units are pure content: they contain no timestamps, no local row ids and no trust level. Trust
//! belongs to the source ([`SourceDescriptor`]); versioning belongs to [`UnitVersion`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Result;
use super::hash::ContentHash;
use super::hlc::Hlc;
use super::ids::{DeviceId, EdgeId, NodeId, NodeKey, SourceId, UnitKey};
use super::kinds::{ChunkKind, EdgeKind, NodeKind, SourceKind, TrustLevel, UnitKind};

/// Free-form attributes of a node or edge. See [`crate::base::attrs`] for well-known names.
pub type Attrs = BTreeMap<String, Value>;

/// A configured source as the graph sees it. The trust level of everything a source asserts is the
/// trust level recorded here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDescriptor {
    /// Source identifier; also the sync namespace.
    pub id: SourceId,
    /// What kind of system the source is.
    pub kind: SourceKind,
    /// Human-readable name.
    pub name: String,
    /// Authority of the source.
    pub trust: TrustLevel,
}

/// A unit's claim that a node exists.
///
/// Several units may assert the same node (the same person appears in the org model, in a
/// CODEOWNERS file and as a ticket assignee). The store merges all assertions of a node: the
/// attributes asserted by the most trusted source win, less trusted sources only fill gaps.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeAssertion {
    /// Canonical key, built with [`crate::base::keys`].
    pub key: NodeKey,
    /// Node kind.
    pub kind: NodeKind,
    /// Display name.
    pub name: String,
    /// Other names this entity is known by (handles, e-mail addresses, titles, short names).
    /// Used for entity resolution; normalised with [`crate::base::keys::normalize_alias`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Kind-specific attributes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: Attrs,
}

impl NodeAssertion {
    /// Creates an assertion without aliases or attributes.
    pub fn new(kind: NodeKind, key: impl Into<NodeKey>, name: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            kind,
            name: name.into(),
            aliases: Vec::new(),
            attrs: Attrs::new(),
        }
    }

    /// Adds an alias (builder style). Empty aliases are ignored.
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        let alias = alias.into();
        if !alias.trim().is_empty() && !self.aliases.contains(&alias) {
            self.aliases.push(alias);
        }
        self
    }

    /// Sets an attribute (builder style).
    pub fn attr(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.attrs.insert(name.into(), value.into());
        self
    }

    /// Sets an attribute only when `value` is `Some` (builder style).
    pub fn attr_opt(self, name: impl Into<String>, value: Option<impl Into<Value>>) -> Self {
        match value {
            Some(value) => self.attr(name, value),
            None => self,
        }
    }
}

/// Narrows a [`Lookup`] to candidates close to where the reference was written.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LookupScope {
    /// Prefer candidates asserted by this source (e.g. the repository containing the call site).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceId>,
    /// Prefer candidates in or near this repository-relative path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// A reference to a node by name rather than by key; resolved by the store's linker.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Lookup {
    /// Acceptable kinds of the target, in order of preference. Never empty.
    pub kinds: Vec<NodeKind>,
    /// The name as written in the source ("payments-api", "@asmith", "charge", "PAY-123").
    pub text: String,
    /// Optional proximity hint used to disambiguate several candidates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<LookupScope>,
}

impl Lookup {
    /// Looks up `text` among nodes of the given kinds.
    pub fn new(kinds: &[NodeKind], text: impl Into<String>) -> Self {
        Self {
            kinds: kinds.to_vec(),
            text: text.into(),
            scope: None,
        }
    }

    /// Prefers candidates from `source` (builder style).
    pub fn in_source(mut self, source: impl Into<SourceId>) -> Self {
        self.scope.get_or_insert_with(LookupScope::default).source = Some(source.into());
        self
    }

    /// Prefers candidates in or near `path` (builder style).
    pub fn near_path(mut self, path: impl Into<String>) -> Self {
        self.scope.get_or_insert_with(LookupScope::default).path = Some(path.into());
        self
    }
}

/// One endpoint of an asserted edge.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRef {
    /// The canonical key is known. The edge materialises as soon as a node with this key (or with
    /// this key as an alias) exists; until then it is kept pending.
    Key(NodeKey),
    /// Only a name is known. The edge materialises when the linker finds exactly one best
    /// candidate; until then it is kept pending and re-tried whenever nodes appear or disappear.
    Lookup(Lookup),
}

impl From<NodeKey> for NodeRef {
    fn from(key: NodeKey) -> Self {
        NodeRef::Key(key)
    }
}

impl From<&NodeKey> for NodeRef {
    fn from(key: &NodeKey) -> Self {
        NodeRef::Key(key.clone())
    }
}

impl From<Lookup> for NodeRef {
    fn from(lookup: Lookup) -> Self {
        NodeRef::Lookup(lookup)
    }
}

fn default_weight() -> f32 {
    1.0
}

fn is_default_weight(weight: &f32) -> bool {
    (*weight - 1.0).abs() < f32::EPSILON
}

/// A unit's claim that a relationship holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EdgeAssertion {
    /// Tail of the edge.
    pub from: NodeRef,
    /// Relationship.
    pub kind: EdgeKind,
    /// Head of the edge.
    pub to: NodeRef,
    /// Confidence of the producer in this claim, in `(0, 1]`. Structural facts are `1.0`;
    /// heuristically extracted mentions are lower.
    #[serde(default = "default_weight", skip_serializing_if = "is_default_weight")]
    pub weight: f32,
    /// Relationship attributes (e.g. the reason a ticket blocks another).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: Attrs,
}

impl EdgeAssertion {
    /// Creates an edge assertion with weight `1.0` and no attributes.
    pub fn new(from: impl Into<NodeRef>, kind: EdgeKind, to: impl Into<NodeRef>) -> Self {
        Self {
            from: from.into(),
            kind,
            to: to.into(),
            weight: 1.0,
            attrs: Attrs::new(),
        }
    }

    /// Sets the confidence, clamped to `(0, 1]` (builder style).
    pub fn weight(mut self, weight: f32) -> Self {
        self.weight = if weight.is_finite() {
            weight.clamp(0.01, 1.0)
        } else {
            1.0
        };
        self
    }

    /// Sets an attribute (builder style).
    pub fn attr(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.attrs.insert(name.into(), value.into());
        self
    }
}

/// A piece of text attached to a node: what gets full-text indexed, embedded and quoted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// The node this text belongs to. Must be asserted by the same unit.
    pub node: NodeKey,
    /// Position of the chunk among the chunks of `node` within this unit.
    pub ordinal: u32,
    /// What kind of text this is.
    pub kind: ChunkKind,
    /// The text itself.
    pub text: String,
}

impl Chunk {
    /// Creates a chunk.
    pub fn new(
        node: impl Into<NodeKey>,
        ordinal: u32,
        kind: ChunkKind,
        text: impl Into<String>,
    ) -> Self {
        Self {
            node: node.into(),
            ordinal,
            kind,
            text: text.into(),
        }
    }
}

/// The atom of ingestion. Applying a unit replaces everything a previous version of the same unit
/// asserted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    /// Stable key, built with [`crate::base::keys::unit`].
    pub key: UnitKey,
    /// The source this unit was ingested from.
    pub source: SourceId,
    /// What the unit represents.
    pub kind: UnitKind,
    /// Asserted nodes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeAssertion>,
    /// Asserted edges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<EdgeAssertion>,
    /// Text chunks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<Chunk>,
}

impl Unit {
    /// Creates an empty unit.
    pub fn new(key: impl Into<UnitKey>, source: impl Into<SourceId>, kind: UnitKind) -> Self {
        Self {
            key: key.into(),
            source: source.into(),
            kind,
            nodes: Vec::new(),
            edges: Vec::new(),
            chunks: Vec::new(),
        }
    }

    /// Adds a node assertion and returns its key for use in edges and chunks.
    pub fn add_node(&mut self, node: NodeAssertion) -> NodeKey {
        let key = node.key.clone();
        self.nodes.push(node);
        key
    }

    /// Adds an edge assertion.
    pub fn add_edge(&mut self, edge: EdgeAssertion) {
        self.edges.push(edge);
    }

    /// Adds a chunk whose ordinal is the number of chunks already attached to `node`.
    pub fn add_chunk(&mut self, node: &NodeKey, kind: ChunkKind, text: impl Into<String>) {
        let text = text.into();
        if text.trim().is_empty() {
            return;
        }
        let ordinal = self
            .chunks
            .iter()
            .filter(|chunk| &chunk.node == node)
            .count() as u32;
        self.chunks
            .push(Chunk::new(node.clone(), ordinal, kind, text));
    }

    /// `true` when the unit asserts nothing.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty() && self.chunks.is_empty()
    }

    /// Puts the unit into canonical form so that its [`content_hash`](Self::content_hash) does not
    /// depend on the order in which the producer emitted things:
    ///
    /// * nodes are sorted by key; duplicate keys are merged (first name and kind win, aliases are
    ///   unioned, attributes of later duplicates fill gaps);
    /// * edges are sorted by `(from, kind, to)`; duplicates keep the greatest weight;
    /// * chunks are sorted by `(node, ordinal)`; duplicates keep the first;
    /// * aliases are sorted and deduplicated.
    pub fn canonicalize(&mut self) {
        self.nodes.sort_by(|a, b| a.key.cmp(&b.key));
        let mut nodes: Vec<NodeAssertion> = Vec::with_capacity(self.nodes.len());
        for node in self.nodes.drain(..) {
            match nodes.last_mut() {
                Some(previous) if previous.key == node.key => {
                    previous.aliases.extend(node.aliases);
                    for (name, value) in node.attrs {
                        previous.attrs.entry(name).or_insert(value);
                    }
                }
                _ => nodes.push(node),
            }
        }
        for node in &mut nodes {
            node.aliases.retain(|alias| !alias.trim().is_empty());
            node.aliases.sort();
            node.aliases.dedup();
        }
        self.nodes = nodes;

        self.edges.sort_by(|a, b| {
            (&a.from, a.kind, &a.to)
                .cmp(&(&b.from, b.kind, &b.to))
                .then(b.weight.total_cmp(&a.weight))
        });
        self.edges.dedup_by(|later, first| {
            later.from == first.from && later.kind == first.kind && later.to == first.to
        });

        self.chunks
            .sort_by(|a, b| (&a.node, a.ordinal).cmp(&(&b.node, b.ordinal)));
        self.chunks
            .dedup_by(|later, first| later.node == first.node && later.ordinal == first.ordinal);
    }

    /// Canonicalises and returns the unit (builder style); producers call this last.
    pub fn finish(mut self) -> Self {
        self.canonicalize();
        self
    }

    /// BLAKE3 hash of the canonical JSON encoding. Only stable for canonical units; see
    /// [`Self::canonicalize`].
    pub fn content_hash(&self) -> Result<ContentHash> {
        ContentHash::of_canonical(self)
    }
}

/// When and where a version of a unit was produced.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnitVersion {
    /// Hybrid logical timestamp of the version.
    pub hlc: Hlc,
    /// Device that produced the version.
    pub origin: DeviceId,
}

impl UnitVersion {
    /// Creates a version stamp.
    pub fn new(hlc: Hlc, origin: impl Into<DeviceId>) -> Self {
        Self {
            hlc,
            origin: origin.into(),
        }
    }
}

/// Everything about a unit version except its content. This is what Merkle trees are built over
/// and what devices compare to decide which unit contents to exchange.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitMeta {
    /// Unit key.
    pub key: UnitKey,
    /// Source (sync namespace) the unit belongs to.
    pub source: SourceId,
    /// Unit kind.
    pub kind: UnitKind,
    /// Content hash of the canonical unit; [`ContentHash::ZERO`] for tombstones.
    pub hash: ContentHash,
    /// Version timestamp.
    pub hlc: Hlc,
    /// Device that produced this version.
    pub origin: DeviceId,
    /// `true` when this version records the deletion of the unit (a tombstone).
    #[serde(default)]
    pub deleted: bool,
}

impl UnitMeta {
    /// The version stamp of this meta.
    pub fn version(&self) -> UnitVersion {
        UnitVersion {
            hlc: self.hlc,
            origin: self.origin.clone(),
        }
    }

    /// Hash identifying this exact version (content and version stamp). Merkle leaf value.
    pub fn version_hash(&self) -> ContentHash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.key.as_str().as_bytes());
        hasher.update(&[0]);
        hasher.update(self.hash.as_bytes());
        hasher.update(&self.hlc.raw().to_be_bytes());
        hasher.update(self.origin.as_str().as_bytes());
        hasher.update(&[0, u8::from(self.deleted)]);
        ContentHash::from_bytes(*hasher.finalize().as_bytes())
    }

    /// The merge rule for two versions of the same unit: last writer wins by hybrid logical time,
    /// ties broken by origin device and then by content hash, so every device picks the same
    /// winner regardless of the order in which versions arrive.
    pub fn supersedes(&self, other: &UnitMeta) -> bool {
        (self.hlc, &self.origin, &self.hash, self.deleted)
            > (other.hlc, &other.origin, &other.hash, other.deleted)
    }
}

/// A node as the in-memory graph sees it: identity, kind and effective trust only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphNode {
    /// Row id of the node.
    pub id: NodeId,
    /// Node kind.
    pub kind: NodeKind,
    /// Greatest trust level among the sources asserting the node.
    pub trust: TrustLevel,
}

/// An edge as the in-memory graph sees it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Row id of the edge.
    pub id: EdgeId,
    /// Tail node.
    pub from: NodeId,
    /// Head node.
    pub to: NodeId,
    /// Relationship.
    pub kind: EdgeKind,
    /// Greatest trust level among the sources claiming the edge.
    pub trust: TrustLevel,
    /// Greatest confidence among the claims, in `(0, 1]`.
    pub weight: f32,
}

/// The change to the materialised graph caused by one store operation.
///
/// Consumers apply it in this order: `nodes_upserted`, `edges_removed`, `edges_upserted`,
/// `nodes_removed`. Removing a node implies removing every edge incident to it; the store also
/// lists those edges in `edges_removed`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphDelta {
    /// Nodes that were created or whose kind/trust changed.
    pub nodes_upserted: Vec<GraphNode>,
    /// Nodes that no longer exist.
    pub nodes_removed: Vec<NodeId>,
    /// Edges that were created or whose trust/weight changed.
    pub edges_upserted: Vec<GraphEdge>,
    /// Edges that no longer exist.
    pub edges_removed: Vec<EdgeId>,
}

impl GraphDelta {
    /// `true` when the operation did not change the graph.
    pub fn is_empty(&self) -> bool {
        self.nodes_upserted.is_empty()
            && self.nodes_removed.is_empty()
            && self.edges_upserted.is_empty()
            && self.edges_removed.is_empty()
    }

    /// Total number of individual changes.
    pub fn len(&self) -> usize {
        self.nodes_upserted.len()
            + self.nodes_removed.len()
            + self.edges_upserted.len()
            + self.edges_removed.len()
    }

    /// Appends the changes of a later operation. The result is equivalent to applying `self` and
    /// then `later`, provided consumers apply sections in the documented order per original delta;
    /// use it for reporting and batching of independent units, not to reorder dependent changes.
    pub fn extend(&mut self, later: GraphDelta) {
        self.nodes_upserted.extend(later.nodes_upserted);
        self.nodes_removed.extend(later.nodes_removed);
        self.edges_upserted.extend(later.edges_upserted);
        self.edges_removed.extend(later.edges_removed);
    }
}

/// Fixed-width little-endian encoding of graph rows, shared by the store's delta log and the
/// on-disk graph snapshot. Ten bytes per node, thirty per edge.
pub mod graph_codec {
    use super::{EdgeId, EdgeKind, GraphDelta, GraphEdge, GraphNode, NodeId, NodeKind, TrustLevel};
    use crate::base::{Error, Result};

    /// Encoded size of a node.
    pub const NODE_BYTES: usize = 10;
    /// Encoded size of an edge.
    pub const EDGE_BYTES: usize = 30;

    /// Appends a node.
    pub fn put_node(out: &mut Vec<u8>, node: &GraphNode) {
        out.extend_from_slice(&node.id.0.to_le_bytes());
        out.push(node.kind.code());
        out.push(node.trust.code());
    }

    /// Appends an edge.
    pub fn put_edge(out: &mut Vec<u8>, edge: &GraphEdge) {
        out.extend_from_slice(&edge.id.0.to_le_bytes());
        out.extend_from_slice(&edge.from.0.to_le_bytes());
        out.extend_from_slice(&edge.to.0.to_le_bytes());
        out.push(edge.kind.code());
        out.push(edge.trust.code());
        out.extend_from_slice(&edge.weight.to_le_bytes());
    }

    /// Reads values from an encoded buffer, failing on truncation or unknown codes.
    #[derive(Debug)]
    pub struct Reader<'a> {
        bytes: &'a [u8],
    }

    impl<'a> Reader<'a> {
        /// A reader over `bytes`.
        pub fn new(bytes: &'a [u8]) -> Self {
            Self { bytes }
        }

        /// `true` when everything has been read.
        pub fn is_empty(&self) -> bool {
            self.bytes.is_empty()
        }

        /// The next `n` bytes.
        pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
            if self.bytes.len() < n {
                return Err(Error::other("encoded graph data is truncated"));
            }
            let (head, tail) = self.bytes.split_at(n);
            self.bytes = tail;
            Ok(head)
        }

        /// A little-endian `u64`.
        pub fn u64(&mut self) -> Result<u64> {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(self.take(8)?);
            Ok(u64::from_le_bytes(raw))
        }

        fn i64(&mut self) -> Result<i64> {
            Ok(self.u64()? as i64)
        }

        fn u8(&mut self) -> Result<u8> {
            Ok(self.take(1)?[0])
        }

        fn trust(&mut self) -> Result<TrustLevel> {
            let code = self.u8()?;
            TrustLevel::from_code(code)
                .ok_or_else(|| Error::other(format!("unknown trust code {code}")))
        }

        /// A count, checked against the bytes left so a corrupt length cannot allocate wildly.
        pub fn count(&mut self, item_bytes: usize) -> Result<usize> {
            let count = self.u64()? as usize;
            if count.saturating_mul(item_bytes) > self.bytes.len() {
                return Err(Error::other("encoded graph data is truncated"));
            }
            Ok(count)
        }

        /// A node.
        pub fn node(&mut self) -> Result<GraphNode> {
            let id = NodeId(self.i64()?);
            let code = self.u8()?;
            let kind = NodeKind::from_code(code)
                .ok_or_else(|| Error::other(format!("unknown node kind code {code}")))?;
            Ok(GraphNode {
                id,
                kind,
                trust: self.trust()?,
            })
        }

        /// An edge.
        pub fn edge(&mut self) -> Result<GraphEdge> {
            let id = EdgeId(self.i64()?);
            let from = NodeId(self.i64()?);
            let to = NodeId(self.i64()?);
            let code = self.u8()?;
            let kind = EdgeKind::from_code(code)
                .ok_or_else(|| Error::other(format!("unknown edge kind code {code}")))?;
            let trust = self.trust()?;
            let mut raw = [0u8; 4];
            raw.copy_from_slice(self.take(4)?);
            Ok(GraphEdge {
                id,
                from,
                to,
                kind,
                trust,
                weight: f32::from_le_bytes(raw),
            })
        }
    }

    /// Encodes a delta.
    pub fn encode_delta(delta: &GraphDelta) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            32 + delta.nodes_upserted.len() * NODE_BYTES
                + delta.edges_upserted.len() * EDGE_BYTES
                + (delta.nodes_removed.len() + delta.edges_removed.len()) * 8,
        );
        out.extend_from_slice(&(delta.nodes_upserted.len() as u64).to_le_bytes());
        for node in &delta.nodes_upserted {
            put_node(&mut out, node);
        }
        out.extend_from_slice(&(delta.nodes_removed.len() as u64).to_le_bytes());
        for id in &delta.nodes_removed {
            out.extend_from_slice(&id.0.to_le_bytes());
        }
        out.extend_from_slice(&(delta.edges_upserted.len() as u64).to_le_bytes());
        for edge in &delta.edges_upserted {
            put_edge(&mut out, edge);
        }
        out.extend_from_slice(&(delta.edges_removed.len() as u64).to_le_bytes());
        for id in &delta.edges_removed {
            out.extend_from_slice(&id.0.to_le_bytes());
        }
        out
    }

    /// Decodes a delta written by [`encode_delta`].
    pub fn decode_delta(bytes: &[u8]) -> Result<GraphDelta> {
        let mut reader = Reader::new(bytes);
        let mut delta = GraphDelta::default();
        for _ in 0..reader.count(NODE_BYTES)? {
            delta.nodes_upserted.push(reader.node()?);
        }
        for _ in 0..reader.count(8)? {
            delta.nodes_removed.push(NodeId(reader.i64()?));
        }
        for _ in 0..reader.count(EDGE_BYTES)? {
            delta.edges_upserted.push(reader.edge()?);
        }
        for _ in 0..reader.count(8)? {
            delta.edges_removed.push(EdgeId(reader.i64()?));
        }
        if !reader.is_empty() {
            return Err(Error::other("trailing bytes after an encoded graph delta"));
        }
        Ok(delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::keys;

    #[test]
    fn graph_deltas_round_trip_through_the_binary_codec() {
        let delta = GraphDelta {
            nodes_upserted: vec![GraphNode {
                id: NodeId(7),
                kind: NodeKind::Service,
                trust: TrustLevel::SourceOfTruth,
            }],
            nodes_removed: vec![NodeId(3)],
            edges_upserted: vec![GraphEdge {
                id: EdgeId(-2),
                from: NodeId(7),
                to: NodeId(9),
                kind: EdgeKind::DependsOn,
                trust: TrustLevel::ReferenceOnly,
                weight: 0.25,
            }],
            edges_removed: vec![EdgeId(11), EdgeId(12)],
        };
        let bytes = graph_codec::encode_delta(&delta);
        assert_eq!(graph_codec::decode_delta(&bytes).expect("decode"), delta);
        assert!(graph_codec::decode_delta(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(graph_codec::decode_delta(&longer).is_err());
        let mut huge = bytes;
        huge[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(graph_codec::decode_delta(&huge).is_err());
    }

    fn sample_unit() -> Unit {
        let source = SourceId::new("payments-api");
        let mut unit = Unit::new(
            keys::unit(&source, UnitKind::Ownership, "CODEOWNERS"),
            source,
            UnitKind::Ownership,
        );
        let team = unit.add_node(
            NodeAssertion::new(
                NodeKind::Team,
                keys::team("Payments Platform"),
                "Payments Platform",
            )
            .alias("@acme/payments-platform"),
        );
        let service = unit.add_node(NodeAssertion::new(
            NodeKind::Service,
            keys::service("payments-api"),
            "payments-api",
        ));
        unit.add_edge(EdgeAssertion::new(&team, EdgeKind::Owns, &service));
        unit.add_edge(EdgeAssertion::new(
            Lookup::new(&[NodeKind::Person], "@asmith"),
            EdgeKind::MemberOf,
            &team,
        ));
        unit.add_chunk(&service, ChunkKind::Description, "Card payments API");
        unit.add_chunk(&service, ChunkKind::Description, "   ");
        unit
    }

    #[test]
    fn canonical_form_is_independent_of_emission_order() {
        let mut forward = sample_unit();
        let mut reversed = sample_unit();
        reversed.nodes.reverse();
        reversed.edges.reverse();
        forward.canonicalize();
        reversed.canonicalize();
        assert_eq!(forward, reversed);
        assert_eq!(
            forward.content_hash().expect("hash"),
            reversed.content_hash().expect("hash")
        );
    }

    #[test]
    fn canonicalize_merges_duplicates() {
        let mut unit = sample_unit();
        unit.add_node(
            NodeAssertion::new(NodeKind::Team, keys::team("payments platform"), "ignored")
                .alias("payments")
                .attr("email", "pay@example.com"),
        );
        unit.add_edge(
            EdgeAssertion::new(
                keys::team("Payments Platform"),
                EdgeKind::Owns,
                keys::service("payments-api"),
            )
            .weight(0.4),
        );
        unit.canonicalize();
        let teams: Vec<_> = unit
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Team)
            .collect();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].name, "Payments Platform");
        assert_eq!(
            teams[0].aliases,
            vec!["@acme/payments-platform", "payments"]
        );
        assert_eq!(
            teams[0].attrs.get("email"),
            Some(&Value::from("pay@example.com"))
        );
        let owns: Vec<_> = unit
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Owns)
            .collect();
        assert_eq!(owns.len(), 1);
        assert!((owns[0].weight - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn empty_chunks_are_dropped_and_ordinals_are_dense() {
        let unit = sample_unit();
        assert_eq!(unit.chunks.len(), 1);
        assert_eq!(unit.chunks[0].ordinal, 0);
    }

    #[test]
    fn unit_survives_a_json_round_trip() {
        let unit = sample_unit().finish();
        let json = serde_json::to_string(&unit).expect("serialize");
        let back: Unit = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, unit);
        assert_eq!(
            back.content_hash().expect("hash"),
            unit.content_hash().expect("hash")
        );
    }

    #[test]
    fn newer_version_supersedes_older_and_ties_are_deterministic() {
        let base = UnitMeta {
            key: UnitKey::new("s/file/a"),
            source: SourceId::new("s"),
            kind: UnitKind::File,
            hash: ContentHash::of(b"v1"),
            hlc: Hlc::new(100, 0),
            origin: DeviceId::new("aaaa"),
            deleted: false,
        };
        let newer = UnitMeta {
            hash: ContentHash::of(b"v2"),
            hlc: Hlc::new(101, 0),
            ..base.clone()
        };
        assert!(newer.supersedes(&base));
        assert!(!base.supersedes(&newer));
        assert!(!base.supersedes(&base));

        let other_device = UnitMeta {
            origin: DeviceId::new("bbbb"),
            ..base.clone()
        };
        assert!(other_device.supersedes(&base) ^ base.supersedes(&other_device));
        assert_ne!(base.version_hash(), newer.version_hash());
        assert_ne!(base.version_hash(), other_device.version_hash());
    }

    #[test]
    fn delta_reports_size() {
        let mut delta = GraphDelta::default();
        assert!(delta.is_empty());
        delta.extend(GraphDelta {
            nodes_removed: vec![NodeId(1)],
            edges_removed: vec![EdgeId(2), EdgeId(3)],
            ..GraphDelta::default()
        });
        assert_eq!(delta.len(), 3);
        assert!(!delta.is_empty());
    }
}
