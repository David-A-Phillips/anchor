//! Merkle trees over the unit versions of one namespace, and the tree comparison built on them.
//!
//! # Shape
//!
//! The tree is a radix tree with fan-out 16 keyed by the BLAKE3 hash of the unit key: the node at
//! path `p` (a sequence of hex digits) covers every unit whose key hash starts with `p`. Hashing
//! the key spreads units uniformly whatever the keys look like.
//!
//! **Depth policy.** The depth adapts to the population, and the shape is a pure function of the
//! set of entries: a node is a *leaf bucket* holding its entries directly while it covers at most
//! [`BUCKET_MAX`] (32) of them, and an *inner node* with exactly 16 children as soon as it covers
//! more. Ten units are a single bucket (comparing them costs one round trip); 100 000 units give
//! depth 3 and ten million depth 5 or 6, with buckets of about 20 entries on average. Paths never
//! exceed [`MAX_DEPTH`] (64 digits, the whole key hash), where buckets grow without bound; that
//! only happens for colliding BLAKE3 hashes. Insert, update and remove touch one root-to-leaf
//! path, so they cost `O(depth)` hash computations.
//!
//! # Hashes
//!
//! * leaf bucket: `ZERO` when empty, otherwise
//!   `BLAKE3("anchor.merkle.leaf\0" ‖ version_hash₁ ‖ … ‖ version_hashₙ)` with the entries ordered
//!   by key hash and `version_hash` = [`UnitMeta::version_hash`];
//! * inner node: `BLAKE3("anchor.merkle.node\0" ‖ child₀ ‖ … ‖ child₁₅)`.
//!
//! The root of the empty tree is therefore [`ContentHash::ZERO`]. Tombstones are ordinary entries
//! (their meta has `deleted = true`), so deletions propagate like any other version.
//!
//! # Comparison
//!
//! [`DiffSession`] walks a remote tree top-down, asking only for nodes whose hash differs from the
//! local one, and yields the entries that exist on one side only or differ, each with its
//! [`UnitMeta`], so that the direction of every transfer can be decided without fetching unit
//! contents. The session is a sans-I/O state machine; [`diff`] and [`diff_async`] drive it over a
//! [`MerkleSource`] or [`AsyncMerkleSource`] (a local tree in tests, a network peer in production).
//! Every node received is checked against the hash announced by its parent, so an inconsistent
//! peer is detected instead of silently producing a wrong plan.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::base::{ContentHash, Error, Result, SourceId, UnitKey, UnitMeta};

/// Number of children of an inner node.
pub const FANOUT: usize = 16;
/// Largest number of entries a node holds before it splits into [`FANOUT`] children.
pub const BUCKET_MAX: usize = 32;
/// Longest possible node path: one hex digit per nibble of the key hash.
pub const MAX_DEPTH: usize = 64;
/// Most node paths a [`DiffSession`] asks for in one request.
pub const MAX_NODES_PER_REQUEST: usize = 256;

const LEAF_TAG: &[u8] = b"anchor.merkle.leaf\0";
const NODE_TAG: &[u8] = b"anchor.merkle.node\0";

/// The position of a unit key in the tree: the BLAKE3 hash of the key text.
pub fn key_hash(key: &UnitKey) -> ContentHash {
    ContentHash::of(key.as_str().as_bytes())
}

fn nibble(hash: &ContentHash, depth: usize) -> usize {
    let byte = hash.as_bytes()[depth / 2];
    usize::from(if depth % 2 == 0 {
        byte >> 4
    } else {
        byte & 0x0f
    })
}

fn leaf_hash<'a>(version_hashes: impl IntoIterator<Item = &'a ContentHash>) -> ContentHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(LEAF_TAG);
    let mut empty = true;
    for hash in version_hashes {
        empty = false;
        hasher.update(hash.as_bytes());
    }
    if empty {
        ContentHash::ZERO
    } else {
        ContentHash::from_bytes(*hasher.finalize().as_bytes())
    }
}

fn inner_hash<'a>(children: impl IntoIterator<Item = &'a ContentHash>) -> ContentHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(NODE_TAG);
    for hash in children {
        hasher.update(hash.as_bytes());
    }
    ContentHash::from_bytes(*hasher.finalize().as_bytes())
}

/// The address of a tree node: the hex digits (nibbles) leading to it from the root.
///
/// Serialises as a string of lowercase hex digits; the root is the empty string.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodePath(Vec<u8>);

impl NodePath {
    /// The root path.
    pub fn root() -> Self {
        Self(Vec::new())
    }

    /// Builds a path from nibbles; each must be below 16 and there can be at most [`MAX_DEPTH`].
    pub fn from_nibbles(nibbles: &[u8]) -> Result<Self> {
        if nibbles.len() > MAX_DEPTH || nibbles.iter().any(|n| *n >= FANOUT as u8) {
            return Err(Error::invalid("invalid Merkle node path"));
        }
        Ok(Self(nibbles.to_vec()))
    }

    /// Parses a string of hex digits.
    pub fn parse(text: &str) -> Result<Self> {
        let nibbles: Option<Vec<u8>> = text
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect();
        match nibbles {
            Some(nibbles) => Self::from_nibbles(&nibbles),
            None => Err(Error::invalid(format!("invalid Merkle node path `{text}`"))),
        }
    }

    /// The nibbles of the path.
    pub fn nibbles(&self) -> &[u8] {
        &self.0
    }

    /// Depth of the node (0 for the root).
    pub fn depth(&self) -> usize {
        self.0.len()
    }

    /// The path of child `index` (`0..16`). Callers guarantee `index < 16` and depth below
    /// [`MAX_DEPTH`]; both are internal invariants of the tree walk.
    fn child(&self, index: usize) -> Self {
        let mut nibbles = Vec::with_capacity(self.0.len() + 1);
        nibbles.extend_from_slice(&self.0);
        nibbles.push(index as u8);
        Self(nibbles)
    }

    /// `true` when a key with this hash lives below this path.
    pub fn covers(&self, key_hash: &ContentHash) -> bool {
        self.0
            .iter()
            .enumerate()
            .all(|(depth, n)| nibble(key_hash, depth) == usize::from(*n))
    }
}

impl fmt::Display for NodePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for n in &self.0 {
            write!(f, "{n:x}")?;
        }
        Ok(())
    }
}

impl Serialize for NodePath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for NodePath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        NodePath::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// The content of a tree node as exchanged between devices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeBody {
    /// An inner node: the hashes of its 16 children ([`ContentHash::ZERO`] for empty ones).
    Inner {
        /// Child hashes, indexed by nibble.
        children: Vec<ContentHash>,
    },
    /// A leaf bucket: the metadata of every entry below the node, ordered by key hash.
    Leaf {
        /// Entries of the bucket (unit key, content hash, version, tombstone flag).
        entries: Vec<UnitMeta>,
    },
}

/// A tree node as exchanged between devices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerkleNode {
    /// Where the node is.
    pub path: NodePath,
    /// Hash of the node.
    pub hash: ContentHash,
    /// Children or entries.
    #[serde(flatten)]
    pub body: NodeBody,
}

#[derive(Clone, Debug)]
struct Entry {
    key_hash: ContentHash,
    version_hash: ContentHash,
    meta: UnitMeta,
}

impl Entry {
    fn new(meta: UnitMeta) -> Self {
        Self {
            key_hash: key_hash(&meta.key),
            version_hash: meta.version_hash(),
            meta,
        }
    }
}

#[derive(Clone, Debug)]
enum Kind {
    /// Entries sorted by key hash.
    Leaf(Vec<Entry>),
    Inner(Box<[Node; FANOUT]>),
}

#[derive(Clone, Debug)]
struct Node {
    hash: ContentHash,
    count: usize,
    kind: Kind,
}

impl Node {
    fn empty() -> Self {
        Self {
            hash: ContentHash::ZERO,
            count: 0,
            kind: Kind::Leaf(Vec::new()),
        }
    }

    /// Builds the canonical subtree for `entries` (sorted by key hash, unique) at `depth`.
    fn build(entries: Vec<Entry>, depth: usize) -> Self {
        let count = entries.len();
        if count <= BUCKET_MAX || depth >= MAX_DEPTH {
            return Self {
                hash: leaf_hash(entries.iter().map(|e| &e.version_hash)),
                count,
                kind: Kind::Leaf(entries),
            };
        }
        let mut groups: [Vec<Entry>; FANOUT] = std::array::from_fn(|_| Vec::new());
        for entry in entries {
            groups[nibble(&entry.key_hash, depth)].push(entry);
        }
        let children = Box::new(groups.map(|group| Node::build(group, depth + 1)));
        Self {
            hash: inner_hash(children.iter().map(|c| &c.hash)),
            count,
            kind: Kind::Inner(children),
        }
    }

    fn rehash(&mut self) {
        self.hash = match &self.kind {
            Kind::Leaf(entries) => leaf_hash(entries.iter().map(|e| &e.version_hash)),
            Kind::Inner(children) => inner_hash(children.iter().map(|c| &c.hash)),
        };
    }

    fn insert(&mut self, entry: Entry, depth: usize) -> Option<UnitMeta> {
        let previous = match &mut self.kind {
            Kind::Leaf(entries) => {
                match entries.binary_search_by(|e| e.key_hash.cmp(&entry.key_hash)) {
                    Ok(index) => Some(std::mem::replace(&mut entries[index], entry).meta),
                    Err(index) => {
                        entries.insert(index, entry);
                        None
                    }
                }
            }
            Kind::Inner(children) => {
                children[nibble(&entry.key_hash, depth)].insert(entry, depth + 1)
            }
        };
        if previous.is_none() {
            self.count += 1;
        }
        if let Kind::Leaf(entries) = &mut self.kind
            && entries.len() > BUCKET_MAX
            && depth < MAX_DEPTH
        {
            let entries = std::mem::take(entries);
            *self = Node::build(entries, depth);
        } else {
            self.rehash();
        }
        previous
    }

    fn remove(&mut self, key_hash: &ContentHash, depth: usize) -> Option<UnitMeta> {
        let removed = match &mut self.kind {
            Kind::Leaf(entries) => entries
                .binary_search_by(|e| e.key_hash.cmp(key_hash))
                .ok()
                .map(|index| entries.remove(index).meta),
            Kind::Inner(children) => children[nibble(key_hash, depth)].remove(key_hash, depth + 1),
        }?;
        self.count -= 1;
        // Keep the shape canonical: a subtree that fits in one bucket again becomes one.
        if matches!(self.kind, Kind::Inner(_)) && self.count <= BUCKET_MAX {
            let mut entries = Vec::with_capacity(self.count);
            self.drain_into(&mut entries);
            self.kind = Kind::Leaf(entries);
        }
        self.rehash();
        Some(removed)
    }

    fn drain_into(&mut self, out: &mut Vec<Entry>) {
        match std::mem::replace(&mut self.kind, Kind::Leaf(Vec::new())) {
            Kind::Leaf(entries) => out.extend(entries),
            Kind::Inner(children) => {
                let children: [Node; FANOUT] = *children;
                for mut child in children {
                    child.drain_into(out);
                }
            }
        }
    }

    fn get(&self, key_hash: &ContentHash, depth: usize) -> Option<&Entry> {
        match &self.kind {
            Kind::Leaf(entries) => entries
                .binary_search_by(|e| e.key_hash.cmp(key_hash))
                .ok()
                .map(|index| &entries[index]),
            Kind::Inner(children) => children[nibble(key_hash, depth)].get(key_hash, depth + 1),
        }
    }

    fn collect<'a>(&'a self, out: &mut Vec<&'a Entry>) {
        match &self.kind {
            Kind::Leaf(entries) => out.extend(entries),
            Kind::Inner(children) => children.iter().for_each(|child| child.collect(out)),
        }
    }

    fn depth(&self) -> usize {
        match &self.kind {
            Kind::Leaf(_) => 0,
            Kind::Inner(children) => 1 + children.iter().map(Node::depth).max().unwrap_or(0),
        }
    }
}

/// What a path resolves to: a real node, or the part of a leaf bucket below a deeper path.
enum Located<'a> {
    Real(&'a Node),
    Virtual(Vec<&'a Entry>),
}

/// A Merkle tree over `unit key → UnitMeta` for one namespace. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct MerkleTree {
    root: Node,
}

impl Default for MerkleTree {
    fn default() -> Self {
        Self::new()
    }
}

impl MerkleTree {
    /// The empty tree; its root is [`ContentHash::ZERO`].
    pub fn new() -> Self {
        Self {
            root: Node::empty(),
        }
    }

    /// Builds a tree in bulk (`O(n log n)`), equal to inserting the metas one by one. When a key
    /// occurs several times the last occurrence wins.
    pub fn from_metas(metas: impl IntoIterator<Item = UnitMeta>) -> Self {
        let mut entries: Vec<Entry> = metas.into_iter().map(Entry::new).collect();
        // Stable sort, so that among equal keys the last one given ends up last.
        entries.sort_by(|a, b| a.key_hash.cmp(&b.key_hash));
        let mut unique: Vec<Entry> = Vec::with_capacity(entries.len());
        for entry in entries {
            match unique.last_mut() {
                Some(last) if last.key_hash == entry.key_hash => *last = entry,
                _ => unique.push(entry),
            }
        }
        Self {
            root: Node::build(unique, 0),
        }
    }

    /// Inserts or replaces the entry for `meta.key`, returning the previous meta.
    pub fn insert(&mut self, meta: UnitMeta) -> Option<UnitMeta> {
        self.root.insert(Entry::new(meta), 0)
    }

    /// Removes the entry for `key`, returning its meta. Note that a deleted unit normally stays in
    /// the tree as a tombstone meta; removal is for entries that are purged altogether.
    pub fn remove(&mut self, key: &UnitKey) -> Option<UnitMeta> {
        self.root.remove(&key_hash(key), 0)
    }

    /// The meta recorded for `key`.
    pub fn get(&self, key: &UnitKey) -> Option<&UnitMeta> {
        self.root.get(&key_hash(key), 0).map(|entry| &entry.meta)
    }

    /// Number of entries, tombstones included.
    pub fn len(&self) -> usize {
        self.root.count
    }

    /// `true` when the tree has no entries.
    pub fn is_empty(&self) -> bool {
        self.root.count == 0
    }

    /// The root hash; [`ContentHash::ZERO`] for the empty tree.
    pub fn root(&self) -> ContentHash {
        self.root.hash
    }

    /// Depth of the deepest leaf bucket (0 when the root is a bucket).
    pub fn depth(&self) -> usize {
        self.root.depth()
    }

    fn locate(&self, path: &NodePath) -> Located<'_> {
        let mut node = &self.root;
        for n in path.nibbles() {
            match &node.kind {
                Kind::Inner(children) => node = &children[usize::from(*n)],
                Kind::Leaf(entries) => {
                    return Located::Virtual(
                        entries.iter().filter(|e| path.covers(&e.key_hash)).collect(),
                    );
                }
            }
        }
        Located::Real(node)
    }

    /// Hash of the node at `path`.
    ///
    /// Defined for every path: below a leaf bucket it is the hash the bucket's matching entries
    /// would have as a bucket of their own (which is what a larger tree holding the same entries
    /// under that path has there, unless it had to split further), and `ZERO` where nothing lives.
    pub fn node_hash(&self, path: &NodePath) -> ContentHash {
        match self.locate(path) {
            Located::Real(node) => node.hash,
            Located::Virtual(entries) => leaf_hash(entries.iter().map(|e| &e.version_hash)),
        }
    }

    /// The node at `path`: child hashes for an inner node, entries with their metas for a leaf
    /// bucket. Defined for every path, like [`Self::node_hash`].
    pub fn node(&self, path: &NodePath) -> MerkleNode {
        let (hash, body) = match self.locate(path) {
            Located::Real(node) => {
                let body = match &node.kind {
                    Kind::Inner(children) => NodeBody::Inner {
                        children: children.iter().map(|c| c.hash).collect(),
                    },
                    Kind::Leaf(entries) => NodeBody::Leaf {
                        entries: entries.iter().map(|e| e.meta.clone()).collect(),
                    },
                };
                (node.hash, body)
            }
            Located::Virtual(entries) => (
                leaf_hash(entries.iter().map(|e| &e.version_hash)),
                NodeBody::Leaf {
                    entries: entries.iter().map(|e| e.meta.clone()).collect(),
                },
            ),
        };
        MerkleNode {
            path: path.clone(),
            hash,
            body,
        }
    }

    fn entries_below(&self, path: &NodePath) -> Vec<&Entry> {
        match self.locate(path) {
            Located::Real(node) => {
                let mut out = Vec::with_capacity(node.count);
                node.collect(&mut out);
                out
            }
            Located::Virtual(entries) => entries,
        }
    }

    /// The metas of every entry below `path`, ordered by key hash.
    pub fn metas_below(&self, path: &NodePath) -> Vec<UnitMeta> {
        self.entries_below(path)
            .into_iter()
            .map(|e| e.meta.clone())
            .collect()
    }

    /// All metas, ordered by key hash.
    pub fn metas(&self) -> Vec<UnitMeta> {
        self.metas_below(&NodePath::root())
    }
}

/// Something that can serve the tree nodes of a peer, blocking.
pub trait MerkleSource {
    /// Returns the nodes at `paths` of the tree for `namespace`, in the same order. A namespace the
    /// peer does not have is an empty tree, not an error.
    fn fetch_nodes(&mut self, namespace: &SourceId, paths: &[NodePath]) -> Result<Vec<MerkleNode>>;
}

/// Something that can serve the tree nodes of a peer, asynchronously (a network transport).
#[async_trait]
pub trait AsyncMerkleSource: Send {
    /// See [`MerkleSource::fetch_nodes`].
    async fn fetch_nodes(
        &mut self,
        namespace: &SourceId,
        paths: &[NodePath],
    ) -> Result<Vec<MerkleNode>>;
}

/// A single tree answers for whatever namespace is asked.
impl MerkleSource for &MerkleTree {
    fn fetch_nodes(&mut self, _: &SourceId, paths: &[NodePath]) -> Result<Vec<MerkleNode>> {
        Ok(paths.iter().map(|path| self.node(path)).collect())
    }
}

/// An entry present on both sides with different versions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPair {
    /// The local version.
    pub local: UnitMeta,
    /// The remote version.
    pub remote: UnitMeta,
}

/// The difference between a local and a remote tree. Every list is ordered by unit key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeDiff {
    /// Entries only the local tree has.
    pub only_local: Vec<UnitMeta>,
    /// Entries only the remote tree has.
    pub only_remote: Vec<UnitMeta>,
    /// Entries both trees have, in different versions.
    pub differ: Vec<DiffPair>,
    /// Number of remote nodes that had to be fetched.
    pub fetched_nodes: usize,
}

impl TreeDiff {
    /// `true` when the trees hold the same entries.
    pub fn is_empty(&self) -> bool {
        self.only_local.is_empty() && self.only_remote.is_empty() && self.differ.is_empty()
    }

    /// Number of differing entries.
    pub fn len(&self) -> usize {
        self.only_local.len() + self.only_remote.len() + self.differ.len()
    }
}

/// Sans-I/O comparison of a local tree with a remote one.
///
/// ```text
/// while let Some(paths) = session.next_request() {
///     session.respond(fetch(paths))?;
/// }
/// let diff = session.finish();
/// ```
///
/// The number of nodes requested is at most `1 + differences × (depth + 1)`.
#[derive(Debug)]
pub struct DiffSession<'a> {
    local: &'a MerkleTree,
    /// Paths to fetch with the hash their parent announced (`None` for the root).
    pending: Vec<(NodePath, Option<ContentHash>)>,
    in_flight: Vec<(NodePath, Option<ContentHash>)>,
    diff: TreeDiff,
}

impl<'a> DiffSession<'a> {
    /// Starts a comparison against `local`.
    pub fn new(local: &'a MerkleTree) -> Self {
        Self {
            local,
            pending: vec![(NodePath::root(), None)],
            in_flight: Vec::new(),
            diff: TreeDiff::default(),
        }
    }

    /// The next remote nodes to fetch (at most [`MAX_NODES_PER_REQUEST`]), or `None` when the
    /// comparison is complete. Each request must be answered with [`Self::respond`] before the
    /// next one is taken.
    pub fn next_request(&mut self) -> Option<Vec<NodePath>> {
        if !self.in_flight.is_empty() {
            return Some(self.in_flight.iter().map(|(path, _)| path.clone()).collect());
        }
        if self.pending.is_empty() {
            return None;
        }
        let take = self.pending.len().min(MAX_NODES_PER_REQUEST);
        self.in_flight = self.pending.drain(..take).collect();
        Some(self.in_flight.iter().map(|(path, _)| path.clone()).collect())
    }

    /// Feeds the remote nodes answering the last request, in request order.
    ///
    /// Fails with [`Error::Sync`] when the answer does not match the request and with
    /// [`Error::Integrity`] when a node contradicts its own hash or the hash its parent announced.
    pub fn respond(&mut self, nodes: Vec<MerkleNode>) -> Result<()> {
        let requested = std::mem::take(&mut self.in_flight);
        if nodes.len() != requested.len() {
            return Err(Error::sync(format!(
                "peer returned {} tree nodes for {} requested",
                nodes.len(),
                requested.len()
            )));
        }
        self.diff.fetched_nodes += nodes.len();
        for (node, (path, expected)) in nodes.into_iter().zip(requested) {
            if node.path != path {
                return Err(Error::sync(format!(
                    "peer returned tree node `{}` for `{path}`",
                    node.path
                )));
            }
            if expected.is_some_and(|hash| hash != node.hash) {
                return Err(Error::integrity(format!(
                    "tree node `{path}` does not match the hash announced by its parent"
                )));
            }
            self.visit(node)?;
        }
        Ok(())
    }

    fn visit(&mut self, remote: MerkleNode) -> Result<()> {
        let path = remote.path;
        match remote.body {
            NodeBody::Inner { children } => {
                if children.len() != FANOUT || path.depth() >= MAX_DEPTH {
                    return Err(Error::sync(format!("malformed inner tree node `{path}`")));
                }
                if inner_hash(&children) != remote.hash {
                    return Err(Error::integrity(format!(
                        "tree node `{path}` does not match its hash"
                    )));
                }
                if self.local.node_hash(&path) == remote.hash {
                    return Ok(());
                }
                for (index, child_hash) in children.into_iter().enumerate() {
                    let child = path.child(index);
                    if self.local.node_hash(&child) == child_hash {
                        continue;
                    }
                    if child_hash.is_zero() {
                        // Nothing remote below here: no need to ask.
                        self.diff.only_local.extend(self.local.metas_below(&child));
                    } else {
                        self.pending.push((child, Some(child_hash)));
                    }
                }
            }
            NodeBody::Leaf { entries } => {
                let mut remote_entries: Vec<Entry> = entries.into_iter().map(Entry::new).collect();
                remote_entries.sort_by(|a, b| a.key_hash.cmp(&b.key_hash));
                if remote_entries.iter().any(|e| !path.covers(&e.key_hash)) {
                    return Err(Error::sync(format!(
                        "tree node `{path}` lists entries that do not belong to it"
                    )));
                }
                if leaf_hash(remote_entries.iter().map(|e| &e.version_hash)) != remote.hash {
                    return Err(Error::integrity(format!(
                        "tree node `{path}` does not match its hash"
                    )));
                }
                let mut local: BTreeMap<ContentHash, &Entry> = self
                    .local
                    .entries_below(&path)
                    .into_iter()
                    .map(|e| (e.key_hash, e))
                    .collect();
                for entry in remote_entries {
                    match local.remove(&entry.key_hash) {
                        None => self.diff.only_remote.push(entry.meta),
                        Some(mine) if mine.version_hash != entry.version_hash => {
                            self.diff.differ.push(DiffPair {
                                local: mine.meta.clone(),
                                remote: entry.meta,
                            });
                        }
                        Some(_) => {}
                    }
                }
                self.diff
                    .only_local
                    .extend(local.into_values().map(|e| e.meta.clone()));
            }
        }
        Ok(())
    }

    /// The result. Call once [`Self::next_request`] has returned `None`; earlier, the difference
    /// found so far is returned.
    pub fn finish(mut self) -> TreeDiff {
        self.diff.only_local.sort_by(|a, b| a.key.cmp(&b.key));
        self.diff.only_remote.sort_by(|a, b| a.key.cmp(&b.key));
        self.diff
            .differ
            .sort_by(|a, b| a.local.key.cmp(&b.local.key));
        self.diff
    }
}

/// Compares `local` with the tree `remote` serves for `namespace`, fetching only differing
/// subtrees.
pub fn diff(
    local: &MerkleTree,
    namespace: &SourceId,
    remote: &mut impl MerkleSource,
) -> Result<TreeDiff> {
    let mut session = DiffSession::new(local);
    while let Some(paths) = session.next_request() {
        session.respond(remote.fetch_nodes(namespace, &paths)?)?;
    }
    Ok(session.finish())
}

/// Asynchronous form of [`diff`]. The local tree is only read between awaits, so the future holds
/// no lock other than the borrow of `local`.
pub async fn diff_async(
    local: &MerkleTree,
    namespace: &SourceId,
    remote: &mut (impl AsyncMerkleSource + ?Sized),
) -> Result<TreeDiff> {
    let mut session = DiffSession::new(local);
    while let Some(paths) = session.next_request() {
        session.respond(remote.fetch_nodes(namespace, &paths).await?)?;
    }
    Ok(session.finish())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Instant;

    use super::*;
    use crate::base::{DeviceId, Hlc, UnitKind};

    pub(crate) fn meta(index: u32, version: u64) -> UnitMeta {
        UnitMeta {
            key: UnitKey::new(format!("src/file/{index}")),
            source: SourceId::new("src"),
            kind: UnitKind::File,
            hash: ContentHash::of(format!("{index}:{version}").as_bytes()),
            hlc: Hlc::new(version, 0),
            origin: DeviceId::new("dev"),
            deleted: false,
        }
    }

    /// Counts the nodes served, to check that comparisons are minimal.
    pub(crate) struct Counting<S> {
        pub inner: S,
        pub nodes: usize,
        pub requests: usize,
    }

    impl<S> Counting<S> {
        pub fn new(inner: S) -> Self {
            Self {
                inner,
                nodes: 0,
                requests: 0,
            }
        }
    }

    impl<S: MerkleSource> MerkleSource for Counting<S> {
        fn fetch_nodes(&mut self, ns: &SourceId, paths: &[NodePath]) -> Result<Vec<MerkleNode>> {
            self.nodes += paths.len();
            self.requests += 1;
            self.inner.fetch_nodes(ns, paths)
        }
    }

    #[async_trait]
    impl<S: MerkleSource + Send> AsyncMerkleSource for Counting<S> {
        async fn fetch_nodes(
            &mut self,
            ns: &SourceId,
            paths: &[NodePath],
        ) -> Result<Vec<MerkleNode>> {
            MerkleSource::fetch_nodes(self, ns, paths)
        }
    }

    fn ns() -> SourceId {
        SourceId::new("src")
    }

    #[test]
    fn empty_and_single_entry_trees() {
        let mut tree = MerkleTree::new();
        assert_eq!(tree.root(), ContentHash::ZERO);
        assert!(tree.is_empty());
        assert!(matches!(
            tree.node(&NodePath::root()).body,
            NodeBody::Leaf { ref entries } if entries.is_empty()
        ));
        assert_eq!(tree.insert(meta(1, 1)), None);
        let single = tree.root();
        assert_ne!(single, ContentHash::ZERO);
        assert_eq!(tree.get(&meta(1, 1).key), Some(&meta(1, 1)));
        assert_eq!(tree.insert(meta(1, 2)), Some(meta(1, 1)));
        assert_ne!(tree.root(), single);
        assert_eq!(tree.remove(&meta(1, 2).key), Some(meta(1, 2)));
        assert_eq!(tree.remove(&meta(1, 2).key), None);
        assert_eq!(tree.root(), ContentHash::ZERO);

        let other = MerkleTree::from_metas([meta(1, 1)]);
        let diff = diff(&MerkleTree::new(), &ns(), &mut &other).expect("diff");
        assert_eq!(diff.only_remote, vec![meta(1, 1)]);
        assert_eq!(diff.fetched_nodes, 1);
    }

    #[test]
    fn root_is_independent_of_insertion_order() {
        let metas: Vec<UnitMeta> = (0..2_000).map(|i| meta(i, 1)).collect();
        let bulk = MerkleTree::from_metas(metas.clone());
        let mut forward = MerkleTree::new();
        metas.iter().cloned().for_each(|m| {
            forward.insert(m);
        });
        let mut backward = MerkleTree::new();
        metas.iter().rev().cloned().for_each(|m| {
            backward.insert(m);
        });
        assert_eq!(bulk.root(), forward.root());
        assert_eq!(bulk.root(), backward.root());
        assert_eq!(bulk.len(), 2_000);
        assert!(bulk.depth() >= 1);
    }

    #[test]
    fn incremental_updates_equal_rebuilds() {
        // A deterministic pseudo-random walk that repeatedly crosses the split/collapse threshold.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut tree = MerkleTree::new();
        let mut model: BTreeMap<u32, UnitMeta> = BTreeMap::new();
        for step in 0..6_000u64 {
            let index = (next() % 700) as u32;
            // Grow for a while, then shrink back to nothing.
            let grow = step < 3_000;
            if (next() % 4 != 0) == grow {
                let m = meta(index, step);
                assert_eq!(tree.insert(m.clone()), model.insert(index, m));
            } else {
                assert_eq!(tree.remove(&meta(index, 0).key), model.remove(&index));
            }
            if step % 97 == 0 {
                let rebuilt = MerkleTree::from_metas(model.values().cloned());
                assert_eq!(tree.root(), rebuilt.root(), "step {step}");
                assert_eq!(tree.len(), model.len());
                assert_eq!(tree.depth(), rebuilt.depth());
            }
        }
        for index in 0..700 {
            tree.remove(&meta(index, 0).key);
        }
        assert_eq!(tree.root(), ContentHash::ZERO);
    }

    #[test]
    fn diff_is_correct_and_minimal() {
        let base: Vec<UnitMeta> = (0..10_000).map(|i| meta(i, 1)).collect();
        let local = MerkleTree::from_metas(base.clone());
        let mut remote = MerkleTree::from_metas(base);

        let mut same = Counting::new(&remote);
        assert!(diff(&local, &ns(), &mut same).expect("diff").is_empty());
        assert_eq!(same.nodes, 1);

        remote.insert(meta(17, 2));
        remote.remove(&meta(4_242, 1).key);
        remote.insert(meta(20_000, 1));
        let mut counting = Counting::new(&remote);
        let result = diff(&local, &ns(), &mut counting).expect("diff");
        assert_eq!(result.only_local, vec![meta(4_242, 1)]);
        assert_eq!(result.only_remote, vec![meta(20_000, 1)]);
        assert_eq!(
            result.differ,
            vec![DiffPair {
                local: meta(17, 1),
                remote: meta(17, 2)
            }]
        );
        let depth = local.depth().max(remote.depth());
        assert!(
            counting.nodes <= 1 + 3 * (depth + 1),
            "{} nodes",
            counting.nodes
        );
        assert_eq!(result.fetched_nodes, counting.nodes);
        // Level by level: one request per level, not one per node.
        assert!(counting.requests <= depth + 1);
    }

    #[test]
    fn diff_handles_trees_of_different_shapes() {
        // A bucket against a deep tree, in both directions, and against an empty tree.
        let small = MerkleTree::from_metas((0..20).map(|i| meta(i, 1)));
        let large = MerkleTree::from_metas((10..5_000).map(|i| meta(i, 1)));
        let forward = diff(&small, &ns(), &mut &large).expect("diff");
        assert_eq!(forward.only_local.len(), 10);
        assert_eq!(forward.only_remote.len(), 4_980);
        assert!(forward.differ.is_empty());
        let backward = diff(&large, &ns(), &mut &small).expect("diff");
        assert_eq!(backward.only_local.len(), 4_980);
        assert_eq!(backward.only_remote.len(), 10);
        assert_eq!(backward.fetched_nodes, 1);
        let empty = diff(&large, &ns(), &mut &MerkleTree::new()).expect("diff");
        assert_eq!(empty.only_local.len(), 4_990);
        assert_eq!(empty.fetched_nodes, 1);
    }

    #[test]
    fn diff_rejects_inconsistent_peers() {
        struct Liar(MerkleTree);
        impl MerkleSource for Liar {
            fn fetch_nodes(&mut self, _: &SourceId, paths: &[NodePath]) -> Result<Vec<MerkleNode>> {
                Ok(paths
                    .iter()
                    .map(|path| {
                        let mut node = self.0.node(path);
                        if let NodeBody::Leaf { entries } = &mut node.body {
                            entries.pop();
                        }
                        node
                    })
                    .collect())
            }
        }
        let local = MerkleTree::from_metas((0..500).map(|i| meta(i, 1)));
        let remote = MerkleTree::from_metas((0..500).map(|i| meta(i, 2)));
        let err = diff(&local, &ns(), &mut Liar(remote)).expect_err("must fail");
        assert!(matches!(err, Error::Integrity(_)), "{err}");
    }

    #[test]
    fn node_paths_round_trip() {
        let path = NodePath::parse("a3f0").expect("path");
        assert_eq!(path.nibbles(), &[10, 3, 15, 0]);
        let json = serde_json::to_string(&path).expect("json");
        assert_eq!(json, "\"a3f0\"");
        assert_eq!(
            serde_json::from_str::<NodePath>(&json).expect("parse"),
            path
        );
        assert!(NodePath::parse("xyz").is_err());
        assert!(NodePath::parse(&"0".repeat(MAX_DEPTH + 1)).is_err());
        let tree = MerkleTree::from_metas((0..100).map(|i| meta(i, 1)));
        let node = tree.node(&NodePath::parse("7").expect("path"));
        let json = serde_json::to_string(&node).expect("json");
        assert_eq!(
            serde_json::from_str::<MerkleNode>(&json).expect("parse"),
            node
        );
    }

    #[test]
    fn large_trees_build_quickly() {
        let started = Instant::now();
        let metas: Vec<UnitMeta> = (0..100_000).map(|i| meta(i, 1)).collect();
        let bulk = MerkleTree::from_metas(metas.clone());
        let mut incremental = MerkleTree::new();
        for m in metas {
            incremental.insert(m);
        }
        assert_eq!(bulk.root(), incremental.root());
        assert_eq!(bulk.len(), 100_000);
        assert!((2..=5).contains(&bulk.depth()), "depth {}", bulk.depth());
        assert!(
            started.elapsed().as_secs() < 10,
            "took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn async_diff_matches_sync_diff() {
        let local = MerkleTree::from_metas((0..3_000).map(|i| meta(i, 1)));
        let remote = MerkleTree::from_metas((5..3_005).map(|i| meta(i, 1 + u64::from(i % 1000 == 0))));
        let expected = diff(&local, &ns(), &mut &remote).expect("diff");
        let mut source = Counting::new(&remote);
        let found = diff_async(&local, &ns(), &mut source).await.expect("diff");
        assert_eq!(found, expected);
        assert_eq!(found.len(), 5 + 5 + 3);
    }
}
