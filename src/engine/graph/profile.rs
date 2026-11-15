//! Traversal profiles: which `(edge kind, direction)` pairs a search may follow, and at what cost.

use serde::{Deserialize, Serialize};

use super::context::{EDGE_KINDS, edge_kind_index, node_kind_index};
use crate::base::{EdgeKind, IntentKind, NodeKind};

/// Direction in which an edge `from --kind--> to` is traversed.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// From the edge's `from` node to its `to` node.
    #[default]
    Forward,
    /// From the edge's `to` node back to its `from` node.
    Reverse,
}

impl Direction {
    /// Both directions.
    pub const ALL: [Direction; 2] = [Direction::Forward, Direction::Reverse];

    /// The opposite direction.
    pub const fn reversed(self) -> Self {
        match self {
            Direction::Forward => Direction::Reverse,
            Direction::Reverse => Direction::Forward,
        }
    }

    const fn index(self) -> usize {
        match self {
            Direction::Forward => 0,
            Direction::Reverse => 1,
        }
    }
}

/// Smallest base cost a profile accepts; smaller (or negative) costs are raised to it so that
/// every step has a strictly positive cost.
const MIN_BASE_COST: f32 = 1e-3;
/// Mask admitting every node kind.
const ANY_KIND: u64 = u64::MAX;

/// One `(edge kind, direction)` rule. `cost` is infinite when the pair is not traversable.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rule {
    cost: f32,
    /// Bit set of node kinds (dense index) that may be entered through this pair.
    into: u64,
}

const CLOSED: Rule = Rule {
    cost: f32::INFINITY,
    into: 0,
};

fn kind_mask(kinds: &[NodeKind]) -> u64 {
    kinds
        .iter()
        .fold(0, |mask, kind| mask | (1u64 << node_kind_index(*kind)))
}

/// For each `(EdgeKind, Direction)` either a base cost or "not traversable".
///
/// A pair may additionally be restricted to steps that *enter* certain node kinds
/// ([`TraversalProfile::allow_into`]): `Contains` in the justification profile only leads into
/// threads, comments and the documents that hold them, not down into every file of a service.
///
/// Base costs must be finite; non-finite costs close the pair and costs below `0.001` are raised
/// to `0.001`.
#[derive(Clone, Debug, PartialEq)]
pub struct TraversalProfile {
    rules: [[Rule; 2]; EDGE_KINDS],
}

impl Default for TraversalProfile {
    /// The empty profile: nothing is traversable.
    fn default() -> Self {
        Self::new()
    }
}

impl TraversalProfile {
    /// A profile in which nothing is traversable; open pairs with the builder methods.
    pub fn new() -> Self {
        Self {
            rules: [[CLOSED; 2]; EDGE_KINDS],
        }
    }

    fn set(mut self, kind: EdgeKind, direction: Direction, cost: f32, into: u64) -> Self {
        self.rules[edge_kind_index(kind)][direction.index()] = if cost.is_finite() {
            Rule {
                cost: cost.max(MIN_BASE_COST),
                into,
            }
        } else {
            CLOSED
        };
        self
    }

    /// Makes `kind` traversable in `direction` at base cost `cost`, into any node kind.
    pub fn allow(self, kind: EdgeKind, direction: Direction, cost: f32) -> Self {
        self.set(kind, direction, cost, ANY_KIND)
    }

    /// Makes `kind` traversable in `direction` at base cost `cost`, but only for steps that enter
    /// a node whose kind is in `into`.
    pub fn allow_into(
        self,
        kind: EdgeKind,
        direction: Direction,
        cost: f32,
        into: &[NodeKind],
    ) -> Self {
        self.set(kind, direction, cost, kind_mask(into))
    }

    /// Makes `kind` traversable in both directions at the same base cost.
    pub fn allow_both(self, kind: EdgeKind, cost: f32) -> Self {
        self.allow(kind, Direction::Forward, cost)
            .allow(kind, Direction::Reverse, cost)
    }

    /// Makes `kind` not traversable in `direction`.
    pub fn deny(mut self, kind: EdgeKind, direction: Direction) -> Self {
        self.rules[edge_kind_index(kind)][direction.index()] = CLOSED;
        self
    }

    /// Base cost of following `kind` in `direction`, or `None` when the pair is not traversable.
    pub fn cost(&self, kind: EdgeKind, direction: Direction) -> Option<f32> {
        let rule = self.rules[edge_kind_index(kind)][direction.index()];
        rule.cost.is_finite().then_some(rule.cost)
    }

    /// Base cost of following `kind` in `direction` into a node of kind `entering`, or `None` when
    /// the pair is closed or does not admit that node kind.
    pub fn step(&self, kind: EdgeKind, direction: Direction, entering: NodeKind) -> Option<f32> {
        let rule = self.rules[edge_kind_index(kind)][direction.index()];
        (rule.cost.is_finite() && rule.into & (1u64 << node_kind_index(entering)) != 0)
            .then_some(rule.cost)
    }

    /// `true` when `kind` may be followed in `direction` (into at least some node kinds).
    pub fn is_traversable(&self, kind: EdgeKind, direction: Direction) -> bool {
        self.cost(kind, direction).is_some()
    }

    /// `true` when no pair is traversable.
    pub fn is_empty(&self) -> bool {
        self.pairs().next().is_none()
    }

    /// Every traversable pair with its base cost, in vocabulary order.
    pub fn pairs(&self) -> impl Iterator<Item = (EdgeKind, Direction, f32)> + '_ {
        EdgeKind::ALL.iter().flat_map(move |kind| {
            Direction::ALL.into_iter().filter_map(move |direction| {
                self.cost(*kind, direction)
                    .map(|cost| (*kind, direction, cost))
            })
        })
    }

    /// "Who owns this?": climbs containment and follows ownership, leadership and membership from
    /// an entity towards its owners, plus implementation links (an API is owned through the
    /// service implementing it). Call chains and references are not followed.
    pub fn ownership() -> Self {
        use Direction::{Forward, Reverse};
        Self::new()
            .allow(EdgeKind::Contains, Reverse, 1.0)
            .allow(EdgeKind::Owns, Reverse, 0.5)
            .allow(EdgeKind::Leads, Reverse, 1.0)
            .allow(EdgeKind::MemberOf, Reverse, 2.0)
            .allow(EdgeKind::Implements, Reverse, 1.0)
            .allow(EdgeKind::Implements, Forward, 2.0)
    }

    /// "Why is this blocked / rejected / dismissed?": approvals, blockers, decisions, discussions
    /// with their threads and comments, tracking tickets, authorship and mentions.
    pub fn justification() -> Self {
        use Direction::{Forward, Reverse};
        Self::new()
            .allow(EdgeKind::Blocks, Reverse, 0.5)
            .allow(EdgeKind::Blocks, Forward, 1.0)
            .allow(EdgeKind::Approves, Reverse, 0.5)
            .allow(EdgeKind::Approves, Forward, 1.0)
            .allow(EdgeKind::DecidedBy, Forward, 1.0)
            .allow(EdgeKind::DecidedBy, Reverse, 3.0)
            .allow_both(EdgeKind::Discusses, 1.0)
            .allow_into(
                EdgeKind::Contains,
                Forward,
                1.0,
                &[NodeKind::Thread, NodeKind::Comment],
            )
            .allow_into(
                EdgeKind::Contains,
                Reverse,
                1.0,
                &[
                    NodeKind::Thread,
                    NodeKind::WikiPage,
                    NodeKind::PullRequest,
                    NodeKind::Ticket,
                ],
            )
            .allow_both(EdgeKind::Tracks, 1.0)
            .allow(EdgeKind::AuthoredBy, Forward, 1.0)
            .allow(EdgeKind::AuthoredBy, Reverse, 3.0)
            .allow_both(EdgeKind::Mentions, 2.0)
    }

    /// "What does this depend on / what depends on it?": calls, references, imports, service
    /// dependencies, API consumption and implementation. Following an edge backwards (impact
    /// analysis) costs a little more than following it forwards.
    pub fn dependency() -> Self {
        use Direction::{Forward, Reverse};
        [
            EdgeKind::Calls,
            EdgeKind::References,
            EdgeKind::Imports,
            EdgeKind::DependsOn,
            EdgeKind::Consumes,
        ]
        .into_iter()
        .fold(Self::new(), |profile, kind| {
            profile
                .allow(kind, Forward, 1.0)
                .allow(kind, Reverse, 1.5)
        })
        .allow_both(EdgeKind::Implements, 1.0)
    }

    /// "Where / how is this defined?": containment, documentation, implementation, and mentions as
    /// weak evidence.
    pub fn definition() -> Self {
        use Direction::{Forward, Reverse};
        Self::new()
            .allow_both(EdgeKind::Contains, 1.0)
            .allow(EdgeKind::Documents, Reverse, 0.5)
            .allow(EdgeKind::Documents, Forward, 1.0)
            .allow_both(EdgeKind::Implements, 1.0)
            .allow_both(EdgeKind::Mentions, 3.0)
    }

    /// "What is the state of this work?": the justification profile plus assignment and
    /// supersession.
    pub fn status() -> Self {
        Self::justification()
            .allow(EdgeKind::AssignedTo, Direction::Forward, 1.0)
            .allow_both(EdgeKind::Supersedes, 1.0)
    }

    /// Everything in both directions: structural and governance edges are cheap, code and work
    /// tracking edges cost more, weak evidence (`Mentions`, `RelatesTo`) is expensive.
    pub fn general() -> Self {
        EdgeKind::ALL.iter().fold(Self::new(), |profile, kind| {
            let cost = match kind {
                EdgeKind::Contains
                | EdgeKind::Owns
                | EdgeKind::MemberOf
                | EdgeKind::Leads
                | EdgeKind::Implements
                | EdgeKind::Documents
                | EdgeKind::Discusses
                | EdgeKind::Approves
                | EdgeKind::DecidedBy
                | EdgeKind::Blocks => 1.0,
                EdgeKind::Mentions | EdgeKind::RelatesTo => 4.0,
                _ => 2.0,
            };
            profile.allow_both(*kind, cost)
        })
    }

    /// The preset matching a classified query intent.
    pub fn for_intent(intent: IntentKind) -> Self {
        match intent {
            IntentKind::Ownership => Self::ownership(),
            IntentKind::Justification => Self::justification(),
            IntentKind::Dependency => Self::dependency(),
            IntentKind::Definition | IntentKind::HowTo => Self::definition(),
            IntentKind::Status => Self::status(),
            IntentKind::General => Self::general(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Direction::{Forward, Reverse};
    use super::*;

    #[test]
    fn builder_opens_closes_and_sanitises() {
        let profile = TraversalProfile::new();
        assert!(profile.is_empty());
        let profile = profile
            .allow(EdgeKind::Calls, Forward, 2.0)
            .allow_both(EdgeKind::Owns, 0.0)
            .allow(EdgeKind::Imports, Forward, f32::NAN)
            .allow_into(EdgeKind::Contains, Forward, 1.0, &[NodeKind::Thread]);
        assert_eq!(profile.cost(EdgeKind::Calls, Forward), Some(2.0));
        assert_eq!(profile.cost(EdgeKind::Calls, Reverse), None);
        assert_eq!(profile.cost(EdgeKind::Owns, Reverse), Some(MIN_BASE_COST));
        assert!(!profile.is_traversable(EdgeKind::Imports, Forward));
        assert_eq!(
            profile.step(EdgeKind::Contains, Forward, NodeKind::Thread),
            Some(1.0)
        );
        assert_eq!(
            profile.step(EdgeKind::Contains, Forward, NodeKind::File),
            None
        );
        assert_eq!(
            profile.step(EdgeKind::Calls, Forward, NodeKind::Symbol),
            Some(2.0)
        );
        assert_eq!(profile.pairs().count(), 4);
        let profile = profile.deny(EdgeKind::Calls, Forward);
        assert!(!profile.is_traversable(EdgeKind::Calls, Forward));
        assert_eq!(Forward.reversed(), Reverse);
        assert_eq!(TraversalProfile::default(), TraversalProfile::new());
    }

    #[test]
    fn presets_follow_the_documented_edges() {
        let ownership = TraversalProfile::ownership();
        // Containment upwards only, owners reached against the edge direction, no call chains.
        assert!(ownership.is_traversable(EdgeKind::Contains, Reverse));
        assert!(!ownership.is_traversable(EdgeKind::Contains, Forward));
        for kind in [EdgeKind::Owns, EdgeKind::Leads, EdgeKind::MemberOf] {
            assert!(ownership.is_traversable(kind, Reverse));
            assert!(!ownership.is_traversable(kind, Forward));
        }
        assert!(ownership.is_traversable(EdgeKind::Implements, Reverse));
        for kind in [EdgeKind::Calls, EdgeKind::References, EdgeKind::Mentions] {
            for direction in Direction::ALL {
                assert!(!ownership.is_traversable(kind, direction));
            }
        }

        let justification = TraversalProfile::justification();
        for kind in [
            EdgeKind::Blocks,
            EdgeKind::Approves,
            EdgeKind::DecidedBy,
            EdgeKind::Discusses,
            EdgeKind::Contains,
            EdgeKind::Tracks,
            EdgeKind::AuthoredBy,
            EdgeKind::Mentions,
        ] {
            assert!(justification.is_traversable(kind, Forward), "{kind}");
        }
        assert!(!justification.is_traversable(EdgeKind::Calls, Forward));
        assert!(
            justification
                .step(EdgeKind::Contains, Forward, NodeKind::Comment)
                .is_some()
        );
        assert!(
            justification
                .step(EdgeKind::Contains, Forward, NodeKind::File)
                .is_none()
        );

        let dependency = TraversalProfile::dependency();
        for kind in [
            EdgeKind::Calls,
            EdgeKind::References,
            EdgeKind::Imports,
            EdgeKind::DependsOn,
            EdgeKind::Consumes,
            EdgeKind::Implements,
        ] {
            assert!(dependency.is_traversable(kind, Forward));
            assert!(dependency.is_traversable(kind, Reverse));
        }
        assert!(!dependency.is_traversable(EdgeKind::Contains, Forward));

        let definition = TraversalProfile::definition();
        for kind in [
            EdgeKind::Contains,
            EdgeKind::Documents,
            EdgeKind::Mentions,
            EdgeKind::Implements,
        ] {
            assert!(definition.is_traversable(kind, Forward));
        }
        assert!(!definition.is_traversable(EdgeKind::Owns, Reverse));

        let general = TraversalProfile::general();
        assert_eq!(general.pairs().count(), EdgeKind::ALL.len() * 2);
        let weak = general.cost(EdgeKind::Mentions, Forward).expect("open");
        let strong = general.cost(EdgeKind::Contains, Forward).expect("open");
        assert!(weak > strong);
    }

    #[test]
    fn intents_map_to_presets() {
        assert_eq!(
            TraversalProfile::for_intent(IntentKind::Ownership),
            TraversalProfile::ownership()
        );
        assert_eq!(
            TraversalProfile::for_intent(IntentKind::Justification),
            TraversalProfile::justification()
        );
        assert_eq!(
            TraversalProfile::for_intent(IntentKind::Dependency),
            TraversalProfile::dependency()
        );
        assert_eq!(
            TraversalProfile::for_intent(IntentKind::Definition),
            TraversalProfile::definition()
        );
        assert_eq!(
            TraversalProfile::for_intent(IntentKind::HowTo),
            TraversalProfile::definition()
        );
        assert_eq!(
            TraversalProfile::for_intent(IntentKind::General),
            TraversalProfile::general()
        );
        let status = TraversalProfile::for_intent(IntentKind::Status);
        assert!(status.is_traversable(EdgeKind::AssignedTo, Forward));
        assert!(status.is_traversable(EdgeKind::Blocks, Reverse));
    }
}
