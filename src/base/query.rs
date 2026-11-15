//! Query-understanding vocabulary shared by the router (which produces it), the query engine (which
//! acts on it) and the backlog (which records it).

use serde::{Deserialize, Serialize};

use super::kinds::NodeKind;
use super::macros::string_enum;

string_enum! {
    /// What the user wants to know.
    pub enum IntentKind {
        /// Who owns / maintains / is responsible for something ("Who owns API X?").
        Ownership = 1 => "ownership",
        /// Why something is the way it is, especially why it is blocked, rejected or dismissed
        /// ("Why is this blocking?").
        Justification = 2 => "justification",
        /// What something depends on, or what depends on it; impact of a change.
        Dependency = 3 => "dependency",
        /// Where or how something is defined or implemented.
        Definition = 4 => "definition",
        /// Current state of work: ticket status, approval status, open threads.
        Status = 5 => "status",
        /// How to do something, according to documentation.
        HowTo = 6 => "how_to",
        /// Anything else.
        General = 7 => "general",
    }
}

string_enum! {
    /// How much reasoning a query needs. Drives the System 1 / System 2 routing decision.
    pub enum Complexity {
        /// A lookup: one hop or a direct attribute.
        Simple = 1 => "simple",
        /// A few hops; can be answered from retrieved facts without synthesis.
        Moderate = 2 => "moderate",
        /// Needs multi-source synthesis, explanation or judgement.
        Complex = 3 => "complex",
    }
}

string_enum! {
    /// What produced an intent analysis.
    pub enum ClassifierKind {
        /// The System 1 language model.
        Model = 1 => "model",
        /// Deterministic rules, used when no local model is available.
        Heuristic = 2 => "heuristic",
    }
}

/// An entity the user referred to in a query.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityMention {
    /// The mention as written ("payments-api", "PAY-123", "POST /v1/charges").
    pub text: String,
    /// The kind the classifier believes the entity has, if it has an opinion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind_hint: Option<NodeKind>,
}

impl EntityMention {
    /// A mention without a kind hint.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind_hint: None,
        }
    }

    /// A mention with a kind hint.
    pub fn with_kind(text: impl Into<String>, kind: NodeKind) -> Self {
        Self {
            text: text.into(),
            kind_hint: Some(kind),
        }
    }
}

/// The result of intent parsing and query classification (System 1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryIntent {
    /// What the user wants to know.
    pub kind: IntentKind,
    /// Entities referred to in the query, in order of appearance.
    #[serde(default)]
    pub entities: Vec<EntityMention>,
    /// Kinds of node that would answer the question (e.g. `[Team, Person]` for ownership).
    #[serde(default)]
    pub target_kinds: Vec<NodeKind>,
    /// How much reasoning the query needs.
    pub complexity: Complexity,
    /// Classifier confidence in `[0, 1]`.
    pub confidence: f32,
    /// What produced this analysis.
    pub classifier: ClassifierKind,
}

impl QueryIntent {
    /// The fallback analysis when nothing better is known.
    pub fn general() -> Self {
        Self {
            kind: IntentKind::General,
            entities: Vec::new(),
            target_kinds: Vec::new(),
            complexity: Complexity::Moderate,
            confidence: 0.0,
            classifier: ClassifierKind::Heuristic,
        }
    }
}
