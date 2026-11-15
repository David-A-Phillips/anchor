//! Trust-weighted pathfinding over a [`ContextGraph`].
//!
//! All searches are Dijkstra searches over petgraph's adjacency in both edge directions, with the
//! lexicographic [`PathCost`]: in strict mode a path using fewer `ReferenceOnly` elements always
//! beats one using more, so authoritative data wins every conflict. Step costs are strictly
//! positive, searches are bounded by hops and cost, and ties are broken by node id, so results are
//! deterministic.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use serde::{Deserialize, Serialize};

use super::config::GraphConfig;
use super::context::ContextGraph;
use super::cost::{CostModel, PathCost};
use super::profile::{Direction, TraversalProfile};
use crate::base::{EdgeId, EdgeKind, NodeId, NodeKind, TrustLevel};

/// Bounds of a search.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    /// Maximum number of edges on a path.
    pub max_hops: u32,
    /// Maximum weighted cost of a path.
    pub max_cost: f32,
}

impl From<&GraphConfig> for Limits {
    fn from(config: &GraphConfig) -> Self {
        Self {
            max_hops: config.max_hops,
            max_cost: config.max_cost,
        }
    }
}

/// A starting point with an initial cost (lower is a better seed).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seed {
    /// The node to start from.
    pub node: NodeId,
    /// Cost already incurred to reach it.
    pub cost: f32,
}

impl Seed {
    /// A seed with zero initial cost.
    pub fn new(node: NodeId) -> Self {
        Self { node, cost: 0.0 }
    }
}

/// What a search is looking for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Goal {
    /// Acceptable node kinds.
    pub kinds: Vec<NodeKind>,
    /// Nodes that never count as goals.
    pub exclude: Vec<NodeId>,
}

impl Goal {
    /// Nodes of any of these kinds.
    pub fn kinds(kinds: &[NodeKind]) -> Self {
        Self {
            kinds: kinds.to_vec(),
            exclude: Vec::new(),
        }
    }
}

/// One edge of a path.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// The edge followed.
    pub edge: EdgeId,
    /// Its kind.
    pub kind: EdgeKind,
    /// Direction travelled.
    pub direction: Direction,
    /// Trust of the edge.
    pub trust: TrustLevel,
}

/// A path through the graph: `nodes.len() == steps.len() + 1`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Path {
    /// Nodes from the seed to the goal.
    pub nodes: Vec<NodeId>,
    /// Edges between consecutive nodes.
    pub steps: Vec<Step>,
    /// Total cost.
    pub cost: PathCost,
}

impl Path {
    /// The last node.
    pub fn target(&self) -> Option<NodeId> {
        self.nodes.last().copied()
    }
}

/// A node reached by [`Pathfinder::expand`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reached {
    /// The node.
    pub node: NodeId,
    /// Cost of the cheapest path to it.
    pub cost: PathCost,
    /// Number of edges on that path.
    pub hops: u32,
}

/// Outcome of resolving competing candidate answers.
#[derive(Clone, Debug, PartialEq)]
pub struct ConflictResolution {
    /// The winning candidate.
    pub winner: Path,
    /// The candidates it overrides, best first.
    pub overridden: Vec<Path>,
    /// `true` when the winner uses no `ReferenceOnly` element.
    pub authoritative: bool,
}

#[derive(Clone, Copy)]
struct Visit {
    cost: PathCost,
    hops: u32,
    previous: Option<(NodeId, Step)>,
}

struct QueueItem {
    cost: PathCost,
    node: NodeId,
    strict: bool,
}

impl PartialEq for QueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for QueueItem {}
impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap: reverse so the cheapest item (then the smallest id) pops first.
        other
            .cost
            .compare(&self.cost, self.strict)
            .then_with(|| other.node.cmp(&self.node))
    }
}

/// Runs searches on a graph with a given trust weighting.
pub struct Pathfinder<'a> {
    graph: &'a ContextGraph,
    model: CostModel,
}

impl<'a> Pathfinder<'a> {
    /// A pathfinder using the trust weights of `config`.
    pub fn new(graph: &'a ContextGraph, config: &GraphConfig) -> Self {
        Self {
            graph,
            model: CostModel::new(&config.trust),
        }
    }

    fn search(
        &self,
        seeds: &[Seed],
        profile: &TraversalProfile,
        limits: Limits,
        mut on_settled: impl FnMut(NodeId, &Visit) -> bool,
    ) -> HashMap<NodeId, Visit> {
        let strict = self.model.strict;
        let mut best: HashMap<NodeId, Visit> = HashMap::new();
        let mut settled: HashMap<NodeId, Visit> = HashMap::new();
        let mut queue = BinaryHeap::new();
        for seed in seeds {
            if !self.graph.contains(seed.node) || !seed.cost.is_finite() {
                continue;
            }
            let cost = PathCost::new(0, seed.cost.max(0.0));
            let better = best
                .get(&seed.node)
                .is_none_or(|visit| cost.compare(&visit.cost, strict) == Ordering::Less);
            if better {
                best.insert(
                    seed.node,
                    Visit {
                        cost,
                        hops: 0,
                        previous: None,
                    },
                );
                queue.push(QueueItem {
                    cost,
                    node: seed.node,
                    strict,
                });
            }
        }
        while let Some(item) = queue.pop() {
            if settled.contains_key(&item.node) {
                continue;
            }
            let Some(visit) = best.get(&item.node).copied() else {
                continue;
            };
            if visit.cost.compare(&item.cost, strict) != Ordering::Equal {
                continue;
            }
            settled.insert(item.node, visit);
            if !on_settled(item.node, &visit) {
                break;
            }
            if visit.hops >= limits.max_hops {
                continue;
            }
            for neighbor in self.graph.neighbors(item.node) {
                if settled.contains_key(&neighbor.node) {
                    continue;
                }
                let Some(entered) = self.graph.node(neighbor.node) else {
                    continue;
                };
                let Some(base) = profile.step(neighbor.edge.kind, neighbor.direction, entered.kind)
                else {
                    continue;
                };
                let weighted = self.model.step_cost(
                    base,
                    neighbor.edge.trust,
                    entered.trust,
                    neighbor.edge.weight,
                );
                let reference = u32::from(neighbor.edge.trust == TrustLevel::ReferenceOnly)
                    + u32::from(entered.trust == TrustLevel::ReferenceOnly);
                let cost = visit.cost + PathCost::new(reference, weighted);
                if cost.weighted > limits.max_cost {
                    continue;
                }
                let better = best
                    .get(&neighbor.node)
                    .is_none_or(|known| cost.compare(&known.cost, strict) == Ordering::Less);
                if better {
                    best.insert(
                        neighbor.node,
                        Visit {
                            cost,
                            hops: visit.hops + 1,
                            previous: Some((
                                item.node,
                                Step {
                                    edge: neighbor.edge.id,
                                    kind: neighbor.edge.kind,
                                    direction: neighbor.direction,
                                    trust: neighbor.edge.trust,
                                },
                            )),
                        },
                    );
                    queue.push(QueueItem {
                        cost,
                        node: neighbor.node,
                        strict,
                    });
                }
            }
        }
        settled
    }

    fn build_path(settled: &HashMap<NodeId, Visit>, target: NodeId) -> Option<Path> {
        let cost = settled.get(&target)?.cost;
        let mut nodes = vec![target];
        let mut steps = Vec::new();
        let mut current = target;
        while let Some((previous, step)) = settled.get(&current)?.previous {
            nodes.push(previous);
            steps.push(step);
            current = previous;
        }
        nodes.reverse();
        steps.reverse();
        Some(Path { nodes, steps, cost })
    }

    /// The cheapest path from `from` to `to`, if one exists within the limits.
    pub fn shortest_path(
        &self,
        from: NodeId,
        to: NodeId,
        profile: &TraversalProfile,
        limits: Limits,
    ) -> Option<Path> {
        let settled = self.search(&[Seed::new(from)], profile, limits, |node, _| node != to);
        Self::build_path(&settled, to)
    }

    /// Up to `k` cheapest paths from the seeds to distinct goal nodes, cheapest first. Seeds
    /// themselves count as goals when they match.
    pub fn nearest(
        &self,
        seeds: &[Seed],
        goal: &Goal,
        k: usize,
        profile: &TraversalProfile,
        limits: Limits,
    ) -> Vec<Path> {
        if k == 0 {
            return Vec::new();
        }
        let mut found = Vec::new();
        let settled = self.search(seeds, profile, limits, |node, _| {
            let matches = !goal.exclude.contains(&node)
                && self
                    .graph
                    .node(node)
                    .is_some_and(|data| goal.kinds.contains(&data.kind));
            if matches {
                found.push(node);
            }
            found.len() < k
        });
        found
            .into_iter()
            .filter_map(|node| Self::build_path(&settled, node))
            .collect()
    }

    /// The nodes reachable from the seeds, cheapest first, at most `max_nodes` of them.
    pub fn expand(
        &self,
        seeds: &[Seed],
        profile: &TraversalProfile,
        limits: Limits,
        max_nodes: usize,
    ) -> Vec<Reached> {
        let mut reached = Vec::new();
        if max_nodes == 0 {
            return reached;
        }
        self.search(seeds, profile, limits, |node, visit| {
            reached.push(Reached {
                node,
                cost: visit.cost,
                hops: visit.hops,
            });
            reached.len() < max_nodes
        });
        reached
    }

    /// Picks the winner among candidate answers to a single-valued question: the cheapest path
    /// under the trust rule (ties broken by target id); the others are reported as overridden.
    pub fn resolve_conflict(&self, mut candidates: Vec<Path>) -> Option<ConflictResolution> {
        let strict = self.model.strict;
        candidates.sort_by(|a, b| {
            a.cost
                .compare(&b.cost, strict)
                .then_with(|| a.target().cmp(&b.target()))
        });
        let mut candidates = candidates.into_iter();
        let winner = candidates.next()?;
        Some(ConflictResolution {
            authoritative: winner.cost.is_authoritative(),
            overridden: candidates.collect(),
            winner,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::{GraphDelta, GraphEdge, GraphNode};

    const SOT: TrustLevel = TrustLevel::SourceOfTruth;
    const REF: TrustLevel = TrustLevel::ReferenceOnly;

    fn node(id: i64, kind: NodeKind, trust: TrustLevel) -> GraphNode {
        GraphNode {
            id: NodeId(id),
            kind,
            trust,
        }
    }

    fn edge(id: i64, from: i64, to: i64, kind: EdgeKind, trust: TrustLevel) -> GraphEdge {
        GraphEdge {
            id: EdgeId(id),
            from: NodeId(from),
            to: NodeId(to),
            kind,
            trust,
            weight: 1.0,
        }
    }

    /// api(1) in service(2) in component(3); team 10 owns the component (authoritative, far);
    /// team 11 "owns" the api directly according to a wiki (reference only, near).
    fn conflict_graph() -> ContextGraph {
        ContextGraph::from_parts(
            [
                node(1, NodeKind::ApiSpec, SOT),
                node(2, NodeKind::Service, SOT),
                node(3, NodeKind::Component, SOT),
                node(10, NodeKind::Team, SOT),
                node(11, NodeKind::Team, SOT),
            ],
            [
                edge(1, 2, 1, EdgeKind::Implements, SOT),
                edge(2, 3, 2, EdgeKind::Contains, SOT),
                edge(3, 10, 3, EdgeKind::Owns, SOT),
                edge(4, 11, 1, EdgeKind::Owns, REF),
            ],
        )
    }

    fn limits() -> Limits {
        Limits::from(&GraphConfig::default())
    }

    #[test]
    fn authoritative_owner_beats_nearer_reference_claim() {
        let graph = conflict_graph();
        let config = GraphConfig::default();
        let finder = Pathfinder::new(&graph, &config);
        let paths = finder.nearest(
            &[Seed::new(NodeId(1))],
            &Goal::kinds(&[NodeKind::Team]),
            5,
            &TraversalProfile::ownership(),
            limits(),
        );
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].target(), Some(NodeId(10)));
        assert!(paths[0].cost.is_authoritative());
        assert_eq!(paths[1].target(), Some(NodeId(11)));
        let resolved = finder.resolve_conflict(paths).expect("candidates");
        assert!(resolved.authoritative);
        assert_eq!(resolved.winner.target(), Some(NodeId(10)));
        assert_eq!(resolved.overridden.len(), 1);
    }

    #[test]
    fn without_strict_authority_only_weighted_cost_counts() {
        let graph = conflict_graph();
        let mut config = GraphConfig::default();
        config.trust.strict_authority = false;
        config.trust.reference_only_multiplier = 1.5;
        let finder = Pathfinder::new(&graph, &config);
        let paths = finder.nearest(
            &[Seed::new(NodeId(1))],
            &Goal::kinds(&[NodeKind::Team]),
            1,
            &TraversalProfile::ownership(),
            limits(),
        );
        assert_eq!(paths[0].target(), Some(NodeId(11)));
    }

    #[test]
    fn reference_claim_is_the_fallback_when_nothing_authoritative_exists() {
        let mut graph = conflict_graph();
        graph.apply(&GraphDelta {
            edges_removed: vec![EdgeId(3)],
            ..GraphDelta::default()
        });
        let config = GraphConfig::default();
        let finder = Pathfinder::new(&graph, &config);
        let paths = finder.nearest(
            &[Seed::new(NodeId(1))],
            &Goal::kinds(&[NodeKind::Team]),
            3,
            &TraversalProfile::ownership(),
            limits(),
        );
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].target(), Some(NodeId(11)));
        assert!(!paths[0].cost.is_authoritative());
    }

    #[test]
    fn shortest_path_reports_steps_and_respects_limits() {
        let graph = conflict_graph();
        let config = GraphConfig::default();
        let finder = Pathfinder::new(&graph, &config);
        let general = TraversalProfile::general();
        let path = finder
            .shortest_path(NodeId(1), NodeId(10), &general, limits())
            .expect("path");
        assert_eq!(
            path.nodes,
            vec![NodeId(1), NodeId(2), NodeId(3), NodeId(10)]
        );
        assert_eq!(path.steps.len(), 3);
        assert!(
            path.steps
                .iter()
                .all(|step| step.direction == Direction::Reverse)
        );
        let tight = Limits {
            max_hops: 2,
            max_cost: 64.0,
        };
        assert!(
            finder
                .shortest_path(NodeId(1), NodeId(10), &general, tight)
                .is_none()
        );
        assert!(
            finder
                .shortest_path(NodeId(1), NodeId(99), &general, limits())
                .is_none()
        );
    }

    #[test]
    fn deltas_keep_the_graph_consistent() {
        let mut graph = conflict_graph();
        assert_eq!((graph.node_count(), graph.edge_count()), (5, 4));
        graph.apply(&GraphDelta {
            nodes_upserted: vec![
                node(11, NodeKind::Team, REF),
                node(12, NodeKind::Person, SOT),
            ],
            edges_upserted: vec![
                edge(5, 12, 10, EdgeKind::Leads, SOT),
                edge(6, 12, 99, EdgeKind::Leads, SOT),
            ],
            nodes_removed: vec![NodeId(3)],
            ..GraphDelta::default()
        });
        assert_eq!(graph.node(NodeId(11)).map(|n| n.trust), Some(REF));
        assert_eq!((graph.node_count(), graph.edge_count()), (5, 3));
        assert_eq!(graph.ignored_edges(), 1);
        assert!(graph.edge(EdgeId(2)).is_none() && graph.edge(EdgeId(3)).is_none());
        assert_eq!(graph.neighbors(NodeId(10)).count(), 1);
        let config = GraphConfig::default();
        let reached = Pathfinder::new(&graph, &config).expand(
            &[Seed::new(NodeId(1))],
            &TraversalProfile::general(),
            limits(),
            10,
        );
        assert_eq!(reached[0].node, NodeId(1));
        assert!(
            reached
                .windows(2)
                .all(|pair| { pair[0].cost.compare(&pair[1].cost, true) != Ordering::Greater })
        );
    }
}
