//! The compact in-memory graph: ids, kinds, trust and weights only.

use std::collections::HashMap;
use std::sync::Arc;

use petgraph::Direction as PetDirection;
use petgraph::stable_graph::{EdgeIndex, NodeIndex, StableDiGraph};

use super::profile::Direction;
use crate::base::{EdgeId, EdgeKind, GraphDelta, GraphEdge, GraphNode, NodeId, NodeKind};

/// Number of edge kinds in the vocabulary.
pub(super) const EDGE_KINDS: usize = EdgeKind::ALL.len();

/// Dense index of an edge kind (its position in [`EdgeKind::ALL`]).
pub(super) fn edge_kind_index(kind: EdgeKind) -> usize {
    EdgeKind::ALL
        .iter()
        .position(|candidate| *candidate == kind)
        .unwrap_or(0)
}

/// Dense index of a node kind (its position in [`NodeKind::ALL`]); always below 64.
pub(super) fn node_kind_index(kind: NodeKind) -> usize {
    NodeKind::ALL
        .iter()
        .position(|candidate| *candidate == kind)
        .unwrap_or(0)
}

/// The in-memory graph shared between the engine, the query engine and the dashboard.
pub type SharedGraph = Arc<parking_lot::RwLock<ContextGraph>>;

/// One step out of a node: the edge, the node at its other end and the direction travelled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Neighbor {
    /// The edge followed.
    pub edge: GraphEdge,
    /// The node reached.
    pub node: NodeId,
    /// `Forward` when travelling from the edge's tail to its head.
    pub direction: Direction,
}

/// A petgraph-backed index of the materialised graph, kept in lockstep with the store by applying
/// every [`GraphDelta`].
#[derive(Clone, Debug, Default)]
pub struct ContextGraph {
    graph: StableDiGraph<GraphNode, GraphEdge, u32>,
    nodes: HashMap<NodeId, NodeIndex<u32>>,
    edges: HashMap<EdgeId, EdgeIndex<u32>>,
    ignored_edges: u64,
    epoch: String,
    version: u64,
}

impl ContextGraph {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a graph from node and edge rows (as loaded from the store).
    pub fn from_parts(
        nodes: impl IntoIterator<Item = GraphNode>,
        edges: impl IntoIterator<Item = GraphEdge>,
    ) -> Self {
        let mut graph = Self::new();
        for node in nodes {
            graph.upsert_node(node);
        }
        for edge in edges {
            graph.upsert_edge(edge);
        }
        graph
    }

    /// The store state this graph mirrors: the database's graph epoch and the graph version of
    /// the last delta applied. A graph built by hand has an empty epoch and version 0.
    pub fn stamp(&self) -> (&str, u64) {
        (&self.epoch, self.version)
    }

    /// Records the store state this graph now mirrors.
    pub fn set_stamp(&mut self, epoch: impl Into<String>, version: u64) {
        self.epoch = epoch.into();
        self.version = version;
    }

    /// Wraps the graph in the shared handle.
    pub fn into_shared(self) -> SharedGraph {
        Arc::new(parking_lot::RwLock::new(self))
    }

    /// Applies a store delta: nodes upserted, edges removed, edges upserted, nodes removed.
    pub fn apply(&mut self, delta: &GraphDelta) {
        for node in &delta.nodes_upserted {
            self.upsert_node(*node);
        }
        for edge in &delta.edges_removed {
            if let Some(index) = self.edges.remove(edge) {
                self.graph.remove_edge(index);
            }
        }
        for edge in &delta.edges_upserted {
            self.upsert_edge(*edge);
        }
        for node in &delta.nodes_removed {
            self.remove_node(*node);
        }
    }

    fn upsert_node(&mut self, node: GraphNode) {
        match self.nodes.get(&node.id) {
            Some(index) => {
                if let Some(weight) = self.graph.node_weight_mut(*index) {
                    *weight = node;
                }
            }
            None => {
                let index = self.graph.add_node(node);
                self.nodes.insert(node.id, index);
            }
        }
    }

    fn upsert_edge(&mut self, edge: GraphEdge) {
        let (Some(from), Some(to)) = (self.nodes.get(&edge.from), self.nodes.get(&edge.to)) else {
            self.ignored_edges += 1;
            return;
        };
        let (from, to) = (*from, *to);
        if let Some(index) = self.edges.get(&edge.id).copied() {
            if self.graph.edge_endpoints(index) == Some((from, to)) {
                if let Some(weight) = self.graph.edge_weight_mut(index) {
                    *weight = edge;
                }
                return;
            }
            self.graph.remove_edge(index);
        }
        let index = self.graph.add_edge(from, to, edge);
        self.edges.insert(edge.id, index);
    }

    fn remove_node(&mut self, id: NodeId) {
        let Some(index) = self.nodes.remove(&id) else {
            return;
        };
        let incident: Vec<EdgeId> = self
            .graph
            .edges_directed(index, PetDirection::Outgoing)
            .chain(self.graph.edges_directed(index, PetDirection::Incoming))
            .map(|edge| edge.weight().id)
            .collect();
        for edge in incident {
            self.edges.remove(&edge);
        }
        self.graph.remove_node(index);
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Number of edges.
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    /// Edges skipped because an endpoint was unknown when they were applied.
    pub fn ignored_edges(&self) -> u64 {
        self.ignored_edges
    }

    /// `true` when the node exists.
    pub fn contains(&self, id: NodeId) -> bool {
        self.nodes.contains_key(&id)
    }

    /// The node with this id.
    pub fn node(&self, id: NodeId) -> Option<GraphNode> {
        self.nodes
            .get(&id)
            .and_then(|index| self.graph.node_weight(*index))
            .copied()
    }

    /// The edge with this id.
    pub fn edge(&self, id: EdgeId) -> Option<GraphEdge> {
        self.edges
            .get(&id)
            .and_then(|index| self.graph.edge_weight(*index))
            .copied()
    }

    /// All nodes, in no particular order.
    pub fn nodes(&self) -> impl Iterator<Item = GraphNode> + '_ {
        self.graph.node_weights().copied()
    }

    /// All edges, in no particular order.
    pub fn edges(&self) -> impl Iterator<Item = GraphEdge> + '_ {
        self.graph.edge_weights().copied()
    }

    /// Every step out of `id`, in both directions.
    pub fn neighbors(&self, id: NodeId) -> impl Iterator<Item = Neighbor> + '_ {
        let index = self.nodes.get(&id).copied();
        let outgoing = index.into_iter().flat_map(move |index| {
            self.graph
                .edges_directed(index, PetDirection::Outgoing)
                .map(|edge| Neighbor {
                    edge: *edge.weight(),
                    node: edge.weight().to,
                    direction: Direction::Forward,
                })
        });
        let incoming = index.into_iter().flat_map(move |index| {
            self.graph
                .edges_directed(index, PetDirection::Incoming)
                .map(|edge| Neighbor {
                    edge: *edge.weight(),
                    node: edge.weight().from,
                    direction: Direction::Reverse,
                })
        });
        outgoing.chain(incoming)
    }

    /// Rough steady-state memory use of the graph and its id maps, in bytes.
    pub fn memory_estimate_bytes(&self) -> usize {
        let node = std::mem::size_of::<GraphNode>() + 8 + 24;
        let edge = std::mem::size_of::<GraphEdge>() + 16 + 24;
        self.graph.node_count() * node + self.graph.edge_count() * edge
    }
}
