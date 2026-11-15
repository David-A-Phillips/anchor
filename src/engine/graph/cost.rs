//! Path costs and the trust-weighted step cost model.

use std::cmp::Ordering;
use std::ops::{Add, AddAssign};

use serde::{Deserialize, Serialize};

use super::config::TrustWeights;
use crate::base::TrustLevel;

/// Smallest edge weight used in cost computation. Weights at or below zero (and non-finite ones)
/// are treated as this value: such an edge stays traversable but is very expensive.
pub(super) const MIN_WEIGHT: f32 = 1e-3;
/// Largest edge weight used in cost computation, so one edge cannot make a path almost free.
pub(super) const MAX_WEIGHT: f32 = 1e3;
/// Bounds applied to configured trust multipliers.
const MIN_MULTIPLIER: f32 = 1e-3;
const MAX_MULTIPLIER: f32 = 1e6;

/// The weight actually used for an edge: finite, strictly positive and bounded.
pub(super) fn effective_weight(weight: f32) -> f32 {
    if weight.is_finite() && weight > 0.0 {
        weight.clamp(MIN_WEIGHT, MAX_WEIGHT)
    } else {
        MIN_WEIGHT
    }
}

/// Cost of a path (or of one step of it).
///
/// * `reference_elements` counts the `ReferenceOnly` nodes and edges on the path, the start node
///   included. A path with zero reference elements is built from authoritative data only.
/// * `weighted` is the sum of the step costs (plus the initial cost of the seed, if any).
///
/// The derived [`PartialOrd`] is **lexicographic** (reference elements first, then weighted cost),
/// which is the `strict_authority` ordering; this is the ordering petgraph algorithms see when
/// `PathCost` is used as a `Measure`. Use [`PathCost::compare`] to rank costs under either mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct PathCost {
    /// Number of `ReferenceOnly` nodes and edges on the path.
    pub reference_elements: u32,
    /// Sum of the weighted step costs.
    pub weighted: f32,
}

impl PathCost {
    /// The cost of standing still.
    pub const ZERO: PathCost = PathCost {
        reference_elements: 0,
        weighted: 0.0,
    };

    /// Builds a cost from its two components.
    pub const fn new(reference_elements: u32, weighted: f32) -> Self {
        Self {
            reference_elements,
            weighted,
        }
    }

    /// Total order between two costs. With `strict_authority` the comparison is lexicographic
    /// (fewer `ReferenceOnly` elements first, then weighted cost); without it only the weighted
    /// cost counts.
    pub fn compare(&self, other: &Self, strict_authority: bool) -> Ordering {
        let weighted = self.weighted.total_cmp(&other.weighted);
        if strict_authority {
            self.reference_elements
                .cmp(&other.reference_elements)
                .then(weighted)
        } else {
            weighted
        }
    }

    /// `true` when the path uses no `ReferenceOnly` node or edge.
    pub const fn is_authoritative(&self) -> bool {
        self.reference_elements == 0
    }
}

impl Add for PathCost {
    type Output = PathCost;

    fn add(self, rhs: PathCost) -> PathCost {
        PathCost {
            reference_elements: self
                .reference_elements
                .saturating_add(rhs.reference_elements),
            weighted: self.weighted + rhs.weighted,
        }
    }
}

impl AddAssign for PathCost {
    fn add_assign(&mut self, rhs: PathCost) {
        *self = *self + rhs;
    }
}

/// Sanitised trust weighting: multipliers are always finite and strictly positive, so every step
/// has a strictly positive cost and searches cannot loop for free.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CostModel {
    source_of_truth: f32,
    reference_only: f32,
    /// Whether costs compare lexicographically (reference elements first).
    pub(super) strict: bool,
}

impl CostModel {
    pub(super) fn new(weights: &TrustWeights) -> Self {
        let defaults = TrustWeights::default();
        let sane = |value: f32, fallback: f32| {
            if value.is_finite() && value > 0.0 {
                value.clamp(MIN_MULTIPLIER, MAX_MULTIPLIER)
            } else {
                fallback
            }
        };
        Self {
            source_of_truth: sane(
                weights.source_of_truth_multiplier,
                defaults.source_of_truth_multiplier,
            ),
            reference_only: sane(
                weights.reference_only_multiplier,
                defaults.reference_only_multiplier,
            ),
            strict: weights.strict_authority,
        }
    }

    /// Cost multiplier of a node or edge with the given trust.
    pub(super) fn multiplier(&self, trust: TrustLevel) -> f32 {
        match trust {
            TrustLevel::SourceOfTruth => self.source_of_truth,
            TrustLevel::ReferenceOnly => self.reference_only,
        }
    }

    /// The smaller of the two multipliers (a lower bound for any element).
    #[cfg(test)]
    pub(super) fn min_multiplier(&self) -> f32 {
        self.source_of_truth.min(self.reference_only)
    }

    /// Step cost = base cost × edge multiplier × multiplier of the node entered ÷ edge weight.
    pub(super) fn step_cost(
        &self,
        base: f32,
        edge_trust: TrustLevel,
        node_trust: TrustLevel,
        weight: f32,
    ) -> f32 {
        base * self.multiplier(edge_trust) * self.multiplier(node_trust) / effective_weight(weight)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_measure<M: petgraph::algo::Measure + Copy>() {}

    #[test]
    fn path_cost_is_a_lexicographic_measure() {
        assert_measure::<PathCost>();
        let authoritative_long = PathCost::new(0, 100.0);
        let reference_short = PathCost::new(1, 0.5);
        assert!(authoritative_long < reference_short);
        assert!(PathCost::new(1, 1.0) < PathCost::new(1, 2.0));
        assert_eq!(PathCost::default(), PathCost::ZERO);
        let mut sum = PathCost::new(1, 1.5) + PathCost::new(2, 2.0);
        assert_eq!(sum, PathCost::new(3, 3.5));
        sum += PathCost::new(0, 0.5);
        assert_eq!(sum, PathCost::new(3, 4.0));
        assert!(PathCost::ZERO.is_authoritative() && !sum.is_authoritative());
    }

    #[test]
    fn compare_follows_the_mode() {
        let authoritative_long = PathCost::new(0, 100.0);
        let reference_short = PathCost::new(1, 0.5);
        assert_eq!(
            authoritative_long.compare(&reference_short, true),
            Ordering::Less
        );
        assert_eq!(
            authoritative_long.compare(&reference_short, false),
            Ordering::Greater
        );
        assert_eq!(
            PathCost::new(3, 1.0).compare(&PathCost::new(0, 1.0), false),
            Ordering::Equal
        );
    }

    #[test]
    fn cost_model_sanitises_weights_and_multipliers() {
        let model = CostModel::new(&TrustWeights {
            source_of_truth_multiplier: f32::NAN,
            reference_only_multiplier: -3.0,
            strict_authority: false,
        });
        assert_eq!(model.multiplier(TrustLevel::SourceOfTruth), 1.0);
        assert_eq!(model.multiplier(TrustLevel::ReferenceOnly), 8.0);
        assert!(!model.strict);
        assert_eq!(model.min_multiplier(), 1.0);
        // base 2 × edge 8 × node 1 ÷ weight 0.5
        let step = model.step_cost(
            2.0,
            TrustLevel::ReferenceOnly,
            TrustLevel::SourceOfTruth,
            0.5,
        );
        assert_eq!(step, 32.0);
        assert_eq!(effective_weight(0.0), MIN_WEIGHT);
        assert_eq!(effective_weight(f32::NAN), MIN_WEIGHT);
        assert_eq!(effective_weight(f32::INFINITY), MIN_WEIGHT);
        assert_eq!(effective_weight(1e9), MAX_WEIGHT);
        assert_eq!(effective_weight(0.75), 0.75);
    }
}
