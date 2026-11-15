//! The routing policy: which tier, if any, should answer a query.
//!
//! [`route`] is a pure function of the intent, the retrieval signals and the state of the tiers,
//! so the whole policy can be read and tested as a table:
//!
//! | # | Condition (first match wins) | Decision |
//! |---|------------------------------|----------|
//! | 1 | Nothing was retrieved (`context_tokens == 0`) | no model: there is nothing to reason over |
//! | 2 | No escalation trigger: the intent is not complex, confidence is at least `escalation_confidence`, and there is no unexplained conflict | no model: answer directly from the retrieved facts |
//! | 3 | A trigger fired and System 2 is usable | System 2 |
//! | 4 | A trigger fired, System 2 is not usable, System 1 is configured | System 1 (degraded) |
//! | 5 | A trigger fired and no model is usable | no model |
//!
//! Triggers, in order of precedence: a conflict that needs explaining (candidates disagree and no
//! authoritative source settles it), a complex intent, low retrieval confidence. A conflict that
//! an authoritative source settles is not a trigger: trust-weighted traversal already picked the
//! winner and the answer can cite the overridden claims without a model.

use std::fmt;

use crate::base::{Complexity, ModelTier, QueryIntent};

/// What retrieval found, as far as routing is concerned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetrievalSignals {
    /// Confidence of the retrieved answer in `[0, 1]`.
    pub confidence: f32,
    /// Candidate answers disagree.
    pub has_conflict: bool,
    /// The winning answer rests only on `SourceOfTruth` data.
    pub authoritative: bool,
    /// Size of the assembled context bundle in tokens; zero when nothing was retrieved.
    pub context_tokens: u32,
}

/// Why a query needs a model at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EscalationTrigger {
    /// Sources disagree and no authoritative source settles the matter.
    Conflict,
    /// The intent needs explanation, comparison or synthesis.
    ComplexIntent,
    /// Retrieval confidence is below `RoutingConfig::escalation_confidence`.
    LowConfidence,
}

/// Why System 2 cannot be used right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum System2Block {
    /// No System 2 model is configured.
    NotConfigured,
    /// System 2 is a cloud model and `RoutingConfig::allow_cloud` is off.
    CloudDisallowed,
    /// System 2 is a cloud model and today's spend reached `max_cloud_usd_per_day`.
    BudgetExhausted,
}

/// The reason behind a [`RouteDecision`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RouteReason {
    /// Nothing was retrieved; a model would have nothing to ground its answer in.
    NoContext,
    /// A simple or moderate, confident lookup: answer from the retrieved facts.
    ConfidentLookup,
    /// Escalated to System 2.
    Escalated(EscalationTrigger),
    /// System 2 was called for but cannot be used; System 1 answers instead.
    LocalFallback(EscalationTrigger, System2Block),
    /// A model was called for but none is usable; answer from the retrieved facts.
    NoModel(EscalationTrigger, System2Block),
}

impl fmt::Display for EscalationTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Conflict => "sources conflict without an authoritative answer",
            Self::ComplexIntent => "the question needs reasoning",
            Self::LowConfidence => "retrieval confidence is low",
        })
    }
}

impl fmt::Display for System2Block {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotConfigured => "System 2 is not configured",
            Self::CloudDisallowed => "cloud calls are disabled",
            Self::BudgetExhausted => "the daily cloud budget is exhausted",
        })
    }
}

impl fmt::Display for RouteReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoContext => f.write_str("nothing was retrieved"),
            Self::ConfidentLookup => f.write_str("confident lookup answered from retrieved facts"),
            Self::Escalated(trigger) => write!(f, "{trigger}; escalated to System 2"),
            Self::LocalFallback(trigger, block) => {
                write!(f, "{trigger}, but {block}; using System 1")
            }
            Self::NoModel(trigger, block) => {
                write!(f, "{trigger}, but {block} and no local model is available")
            }
        }
    }
}

/// The routing decision for one query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteDecision {
    /// Tier to synthesise the answer with; `None` means answer directly from the retrieved
    /// facts without calling a model.
    pub tier: Option<ModelTier>,
    /// Why.
    pub reason: RouteReason,
}

/// State of the tiers, as seen by the policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TierAvailability {
    /// A System 1 model is configured.
    pub system1: bool,
    /// `Ok` when System 2 may be called now, otherwise why not.
    pub system2: Result<(), System2Block>,
    /// `RoutingConfig::escalation_confidence`.
    pub escalation_confidence: f32,
}

/// The escalation trigger for a query, if any (see the module documentation).
pub fn escalation_trigger(
    intent: &QueryIntent,
    signals: &RetrievalSignals,
    escalation_confidence: f32,
) -> Option<EscalationTrigger> {
    if signals.has_conflict && !signals.authoritative {
        Some(EscalationTrigger::Conflict)
    } else if intent.complexity == Complexity::Complex {
        Some(EscalationTrigger::ComplexIntent)
    } else if signals.confidence.is_nan() || signals.confidence < escalation_confidence {
        // NaN compares false with everything; treating it as low keeps a broken score safe.
        Some(EscalationTrigger::LowConfidence)
    } else {
        None
    }
}

/// The routing policy (see the module documentation for the decision table).
pub fn route(
    intent: &QueryIntent,
    signals: &RetrievalSignals,
    tiers: &TierAvailability,
) -> RouteDecision {
    let decision = |tier, reason| RouteDecision { tier, reason };
    if signals.context_tokens == 0 {
        return decision(None, RouteReason::NoContext);
    }
    let Some(trigger) = escalation_trigger(intent, signals, tiers.escalation_confidence) else {
        return decision(None, RouteReason::ConfidentLookup);
    };
    match tiers.system2 {
        Ok(()) => decision(Some(ModelTier::System2), RouteReason::Escalated(trigger)),
        Err(block) if tiers.system1 => decision(
            Some(ModelTier::System1),
            RouteReason::LocalFallback(trigger, block),
        ),
        Err(block) => decision(None, RouteReason::NoModel(trigger, block)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::{ClassifierKind, IntentKind};

    fn intent(complexity: Complexity) -> QueryIntent {
        QueryIntent {
            kind: IntentKind::Ownership,
            entities: Vec::new(),
            target_kinds: Vec::new(),
            complexity,
            confidence: 0.9,
            classifier: ClassifierKind::Heuristic,
        }
    }

    #[test]
    fn policy_table() {
        use Complexity::{Complex, Moderate, Simple};
        use EscalationTrigger::{ComplexIntent, Conflict, LowConfidence};
        use ModelTier::{System1, System2};
        use RouteReason::{ConfidentLookup, Escalated, LocalFallback, NoContext, NoModel};
        use System2Block::{BudgetExhausted, CloudDisallowed, NotConfigured};

        // (complexity, confidence, conflict, authoritative, context, s1, s2) -> (tier, reason)
        type Row = (
            Complexity,
            f32,
            bool,
            bool,
            u32,
            bool,
            Result<(), System2Block>,
            Option<ModelTier>,
            RouteReason,
        );
        let table: &[Row] = &[
            // Confident lookups never call a model, whatever is configured.
            (
                Simple,
                0.9,
                false,
                true,
                500,
                true,
                Ok(()),
                None,
                ConfidentLookup,
            ),
            (
                Moderate,
                0.6,
                false,
                false,
                500,
                true,
                Ok(()),
                None,
                ConfidentLookup,
            ),
            (
                Simple,
                0.9,
                true,
                true,
                500,
                true,
                Ok(()),
                None,
                ConfidentLookup,
            ),
            (
                Simple,
                0.9,
                false,
                true,
                500,
                false,
                Err(NotConfigured),
                None,
                ConfidentLookup,
            ),
            // Nothing retrieved: nothing to reason over.
            (Complex, 0.1, true, false, 0, true, Ok(()), None, NoContext),
            // Triggers escalate when System 2 is usable.
            (
                Complex,
                0.9,
                false,
                true,
                500,
                true,
                Ok(()),
                Some(System2),
                Escalated(ComplexIntent),
            ),
            (
                Simple,
                0.59,
                false,
                true,
                500,
                true,
                Ok(()),
                Some(System2),
                Escalated(LowConfidence),
            ),
            (
                Simple,
                f32::NAN,
                false,
                true,
                500,
                true,
                Ok(()),
                Some(System2),
                Escalated(LowConfidence),
            ),
            (
                Simple,
                0.9,
                true,
                false,
                500,
                true,
                Ok(()),
                Some(System2),
                Escalated(Conflict),
            ),
            (
                Complex,
                0.1,
                true,
                false,
                500,
                false,
                Ok(()),
                Some(System2),
                Escalated(Conflict),
            ),
            // System 2 unusable: System 1 when there is one.
            (
                Complex,
                0.9,
                false,
                true,
                500,
                true,
                Err(NotConfigured),
                Some(System1),
                LocalFallback(ComplexIntent, NotConfigured),
            ),
            (
                Simple,
                0.2,
                false,
                true,
                500,
                true,
                Err(CloudDisallowed),
                Some(System1),
                LocalFallback(LowConfidence, CloudDisallowed),
            ),
            (
                Simple,
                0.9,
                true,
                false,
                500,
                true,
                Err(BudgetExhausted),
                Some(System1),
                LocalFallback(Conflict, BudgetExhausted),
            ),
            // No model at all.
            (
                Complex,
                0.9,
                false,
                true,
                500,
                false,
                Err(NotConfigured),
                None,
                NoModel(ComplexIntent, NotConfigured),
            ),
            (
                Simple,
                0.2,
                false,
                true,
                500,
                false,
                Err(BudgetExhausted),
                None,
                NoModel(LowConfidence, BudgetExhausted),
            ),
        ];
        for (
            row,
            &(
                complexity,
                confidence,
                has_conflict,
                authoritative,
                context_tokens,
                system1,
                system2,
                tier,
                reason,
            ),
        ) in table.iter().enumerate()
        {
            let decision = route(
                &intent(complexity),
                &RetrievalSignals {
                    confidence,
                    has_conflict,
                    authoritative,
                    context_tokens,
                },
                &TierAvailability {
                    system1,
                    system2,
                    escalation_confidence: 0.6,
                },
            );
            assert_eq!(decision, RouteDecision { tier, reason }, "row {row}");
            assert!(!decision.reason.to_string().is_empty());
        }
    }
}
