//! Deterministic synthetic enterprise: multi-repository code, an organisation model, wiki pages
//! with comment threads, an API approval portal, Jira issues and code reviews, plus ground-truth
//! question sets with known answers.
//!
//! [`generate`] writes a directory that Anchor ingests as-is (`anchor.toml` lists every source
//! with its trust level and keeps its data directory inside the output). The output is a pure
//! function of the [`SynthSpec`]: it uses its own PRNG, no clock and no hash-map iteration order,
//! so two runs produce byte-identical trees (the absolute path of the output directory appears
//! only in `anchor.toml`).
//!
//! ```text
//! <out>/
//!   anchor.toml          sources with trust levels; System 1 disabled so no model is needed
//!   ground_truth.json    ownership, justification and relationship questions (see [`GroundTruth`])
//!   org/org.yaml         portfolios > domains > components > services, teams, people (source of truth)
//!   legacy/              an old, stale directory export (reference only): the stale-claim source
//!   repos/<service>/     code, README, CODEOWNERS | MAINTAINERS | OWNERS, openapi.yaml
//!   portal/              API records, OpenAPI definitions and approvals (source of truth)
//!   wiki/                Markdown and HTML pages with comment threads (reference only)
//!   jira/                issue exports (reference only)
//!   reviews/             pull-request exports (reference only)
//! ```
//!
//! Ground truth deliberately contains distractors: similarly named services (`payments-api` next
//! to `payments-gateway`), stale wiki pages whose ownership claims contradict the authoritative
//! sources, and people who sit in several teams.

mod code;
mod config;
mod jira;
mod org;
mod plan;
mod portal;
mod questions;
mod repos;
mod reviews;
mod rng;
mod spec;
mod truth;
mod wiki;
mod words;
mod writer;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::base::Result;
use plan::Plan;
use writer::Writer;

pub use spec::{CaseRates, LanguageWeight, Preset, SynthLanguage, SynthSpec};
pub use truth::{
    GroundTruth, JustificationKind, JustificationQuestion, OwnershipKind, OwnershipQuestion,
    OwnershipScenario, PathStep, RelationshipKind, RelationshipQuestion,
};

/// Source id of the authoritative organisation model.
pub const SOURCE_ORG: &str = "enterprise-org";
/// Source id of the stale, reference-only directory export.
pub const SOURCE_LEGACY: &str = "legacy-directory";
/// Source id of the wiki.
pub const SOURCE_WIKI: &str = "eng-wiki";
/// Source id of the API portal.
pub const SOURCE_PORTAL: &str = "api-portal";
/// Source id of the issue tracker.
pub const SOURCE_JIRA: &str = "jira";
/// Source id of the code-review export.
pub const SOURCE_REVIEWS: &str = "code-reviews";

/// What [`generate`] wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynthReport {
    /// Repositories (services), including those outside the org model.
    pub repositories: usize,
    /// Source files per language name.
    pub source_files_by_language: BTreeMap<String, usize>,
    /// Source files in total.
    pub source_files: usize,
    /// Portfolios in the org model.
    pub portfolios: usize,
    /// Domains in the org model.
    pub domains: usize,
    /// Components in the org model.
    pub components: usize,
    /// Teams.
    pub teams: usize,
    /// People.
    pub people: usize,
    /// API records.
    pub apis: usize,
    /// Approval records.
    pub approvals: usize,
    /// Wiki pages.
    pub wiki_pages: usize,
    /// Wiki comment threads.
    pub wiki_threads: usize,
    /// Jira issues.
    pub tickets: usize,
    /// Pull-request exports.
    pub pull_requests: usize,
    /// Bytes written per kind (`repos`, `org`, `legacy`, `wiki`, `portal`, `jira`, `reviews`,
    /// `config`, `ground_truth`).
    pub bytes_by_kind: BTreeMap<String, u64>,
    /// Files written per kind.
    pub files_by_kind: BTreeMap<String, usize>,
    /// Questions per category (`ownership.service`, `justification.ticket_blocked`, ...).
    pub questions_by_category: BTreeMap<String, usize>,
}

impl SynthReport {
    /// Total bytes written.
    pub fn total_bytes(&self) -> u64 {
        self.bytes_by_kind.values().sum()
    }

    /// Total questions.
    pub fn total_questions(&self) -> usize {
        self.questions_by_category.values().sum()
    }
}

/// State shared by the emitters.
pub(crate) struct Ctx<'a> {
    pub spec: &'a SynthSpec,
    pub plan: &'a Plan,
    pub out: Writer,
    pub truth: GroundTruth,
    pub report: SynthReport,
    /// Wiki page id of the stale ownership page, per conflicting service.
    pub stale_pages: BTreeMap<usize, String>,
}

/// Generates the dataset described by `spec` into `out_dir` (created if needed; existing files of
/// the same names are overwritten).
///
/// # Errors
///
/// Fails on I/O errors writing the tree.
pub fn generate(spec: &SynthSpec, out_dir: &Path) -> Result<SynthReport> {
    std::fs::create_dir_all(out_dir)?;
    let root = std::fs::canonicalize(out_dir)?;
    let plan = Plan::build(spec);
    let mut ctx = Ctx {
        spec,
        plan: &plan,
        out: Writer::new(&root),
        truth: GroundTruth {
            seed: spec.seed,
            ..GroundTruth::default()
        },
        report: SynthReport {
            repositories: plan.services.len(),
            portfolios: plan.portfolios.len(),
            domains: plan.domains.len(),
            components: plan
                .components
                .iter()
                .filter(|c| !c.services.is_empty())
                .count(),
            teams: plan.teams.len(),
            people: plan.people.len(),
            tickets: plan.tickets.len(),
            pull_requests: plan.pull_requests.len(),
            ..SynthReport::default()
        },
        stale_pages: BTreeMap::new(),
    };
    org::emit(&mut ctx)?;
    repos::emit(&mut ctx)?;
    portal::emit(&mut ctx)?;
    wiki::emit(&mut ctx)?;
    jira::emit(&mut ctx)?;
    reviews::emit(&mut ctx)?;
    questions::emit(&mut ctx);
    config::emit(&mut ctx, &root)?;

    let Ctx {
        out,
        truth,
        mut report,
        ..
    } = ctx;
    report.questions_by_category = truth.counts();
    let mut out = out;
    let json = serde_json::to_string_pretty(&truth)?;
    out.write("ground_truth", "ground_truth.json", &json)?;
    let (bytes, files) = out.finish();
    report.bytes_by_kind = bytes;
    report.files_by_kind = files;
    Ok(report)
}

/// Next sequential id for a question family, e.g. `own-0007`.
pub(crate) fn question_id(prefix: &str, n: usize) -> String {
    format!("{prefix}-{n:04}")
}
