//! Ground truth: the questions a dataset can answer and the node keys that answer them.
//!
//! Every key is a canonical node key exactly as the ingested graph contains it, so an evaluation
//! is plain string equality against `GraphStore::node_by_key` results.

use serde::{Deserialize, Serialize};

/// What an ownership question asks about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipKind {
    /// "Who owns API X?"
    Api,
    /// "Who owns the X service?"
    Service,
    /// "Which team is responsible for component X?"
    Component,
    /// "Who maintains file X?"
    File,
    /// "Who should I contact about endpoint X?"
    Endpoint,
}

impl OwnershipKind {
    /// Stable lower-case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            OwnershipKind::Api => "api",
            OwnershipKind::Service => "service",
            OwnershipKind::Component => "component",
            OwnershipKind::File => "file",
            OwnershipKind::Endpoint => "endpoint",
        }
    }
}

/// How the answer to an ownership question arises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipScenario {
    /// A source-of-truth source names the owner directly.
    Direct,
    /// The entity has no direct owner; the component containing it does.
    Inherited,
    /// A stale reference-level claim contradicts the authoritative owner.
    Conflict,
    /// Only a reference-level source names an owner (the fallback case).
    ReferenceOnly,
}

/// An ownership question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipQuestion {
    /// Unique id within the dataset.
    pub id: String,
    /// What is asked about.
    pub kind: OwnershipKind,
    /// The question in natural language.
    pub question: String,
    /// Key of the entity the question is about.
    pub subject_key: String,
    /// Keys of the owner nodes (team, or person for MAINTAINERS / OWNERS files).
    pub expected_owner_keys: Vec<String>,
    /// Keys of the leads of the owning team, for "who should I contact" style answers.
    pub expected_lead_keys: Vec<String>,
    /// Keys of the nodes that justify the answer.
    pub evidence_keys: Vec<String>,
    /// Keys of stale claims that must NOT win (the wrongly named team, the stale wiki page).
    pub stale_claim_keys: Vec<String>,
    /// How the answer arises.
    pub scenario: OwnershipScenario,
}

/// What a justification question asks about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JustificationKind {
    /// "Why is `<ticket>` blocked?"
    TicketBlocked,
    /// "Why is `<API>` not approved?"
    ApiNotApproved,
    /// "Why is `<endpoint>` blocked?"
    EndpointBlocked,
    /// "What is blocking `<pull request>`?"
    PullRequestBlocked,
    /// "Why was the review on `<pull request>` dismissed?"
    ReviewDismissed,
    /// "Why was the suggestion on `<page>` dismissed?"
    ThreadDismissed,
}

impl JustificationKind {
    /// Stable lower-case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            JustificationKind::TicketBlocked => "ticket_blocked",
            JustificationKind::ApiNotApproved => "api_not_approved",
            JustificationKind::EndpointBlocked => "endpoint_blocked",
            JustificationKind::PullRequestBlocked => "pull_request_blocked",
            JustificationKind::ReviewDismissed => "review_dismissed",
            JustificationKind::ThreadDismissed => "thread_dismissed",
        }
    }
}

/// A "why" question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JustificationQuestion {
    /// Unique id within the dataset.
    pub id: String,
    /// What is asked about.
    pub kind: JustificationKind,
    /// The question in natural language.
    pub question: String,
    /// Key of the blocked or dismissed entity.
    pub subject_key: String,
    /// Keys of the nodes that constitute the justification.
    pub expected_keys: Vec<String>,
    /// Phrases the rationale text contains.
    pub key_phrases: Vec<String>,
}

/// What a relationship question asks about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipKind {
    /// "Which portfolio does `<service>` belong to?"
    ServicePortfolio,
    /// "Who leads the team that owns the component containing `<service>`?"
    ServiceTeamLead,
    /// "Which services consume `<API>`?"
    ApiConsumers,
    /// "Which domain owns the code that implements `<endpoint>`?"
    EndpointDomain,
}

impl RelationshipKind {
    /// Stable lower-case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            RelationshipKind::ServicePortfolio => "service_portfolio",
            RelationshipKind::ServiceTeamLead => "service_team_lead",
            RelationshipKind::ApiConsumers => "api_consumers",
            RelationshipKind::EndpointDomain => "endpoint_domain",
        }
    }
}

/// One hop of a path: the node reached and the edge that leads there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathStep {
    /// Key of the node reached.
    pub key: String,
    /// `EdgeKind` name of the edge between the previous node and this one (`None` for the start).
    pub edge: Option<String>,
    /// `true` when the edge is directed from the previous node to this one.
    pub forward: bool,
}

/// A multi-hop relationship question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipQuestion {
    /// Unique id within the dataset.
    pub id: String,
    /// What is asked about.
    pub kind: RelationshipKind,
    /// The question in natural language.
    pub question: String,
    /// Key of the node the question starts from.
    pub subject_key: String,
    /// Keys of the correct answers.
    pub expected_keys: Vec<String>,
    /// For each expected key, the path that reaches it from the subject.
    pub paths: Vec<Vec<PathStep>>,
}

/// A question answered by the prose of a page (a runbook step, a design statement).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProseQuestion {
    /// Unique id within the dataset.
    pub id: String,
    /// `runbook_step` or `design_statement`, with `_paraphrased` appended when the question
    /// avoids the page's own wording (what keyword search cannot match).
    pub kind: String,
    /// The question in natural language.
    pub question: String,
    /// Key of the page that answers it.
    pub subject_key: String,
    /// Phrases of the page that an answer must convey.
    pub key_phrases: Vec<String>,
}

/// Everything `ground_truth.json` holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroundTruth {
    /// Seed of the dataset.
    pub seed: u64,
    /// Ownership questions.
    pub ownership: Vec<OwnershipQuestion>,
    /// Justification questions.
    pub justification: Vec<JustificationQuestion>,
    /// Multi-hop relationship questions.
    pub relationships: Vec<RelationshipQuestion>,
    /// Questions answered by page prose.
    #[serde(default)]
    pub prose: Vec<ProseQuestion>,
}

impl GroundTruth {
    /// Every key mentioned anywhere in the ground truth.
    pub fn all_keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = Vec::new();
        for q in &self.ownership {
            keys.push(&q.subject_key);
            keys.extend(q.expected_owner_keys.iter().map(String::as_str));
            keys.extend(q.expected_lead_keys.iter().map(String::as_str));
            keys.extend(q.evidence_keys.iter().map(String::as_str));
            keys.extend(q.stale_claim_keys.iter().map(String::as_str));
        }
        for q in &self.justification {
            keys.push(&q.subject_key);
            keys.extend(q.expected_keys.iter().map(String::as_str));
        }
        for q in &self.relationships {
            keys.push(&q.subject_key);
            keys.extend(q.expected_keys.iter().map(String::as_str));
            keys.extend(q.paths.iter().flatten().map(|step| step.key.as_str()));
        }
        for q in &self.prose {
            keys.push(&q.subject_key);
        }
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    /// Number of questions per category, e.g. `ownership.service` or `justification.ticket_blocked`.
    pub fn counts(&self) -> std::collections::BTreeMap<String, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for q in &self.ownership {
            *counts
                .entry(format!("ownership.{}", q.kind.as_str()))
                .or_insert(0) += 1;
        }
        for q in &self.justification {
            *counts
                .entry(format!("justification.{}", q.kind.as_str()))
                .or_insert(0) += 1;
        }
        for q in &self.relationships {
            *counts
                .entry(format!("relationship.{}", q.kind.as_str()))
                .or_insert(0) += 1;
        }
        for q in &self.prose {
            *counts.entry(format!("prose.{}", q.kind)).or_insert(0) += 1;
        }
        counts
    }
}
