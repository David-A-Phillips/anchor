//! The closed vocabularies of the context graph.
//!
//! Textual names and numeric codes are part of the on-disk and wire formats. Append only.

use serde::{Deserialize, Serialize};

use super::macros::string_enum;

/// How much authority a source carries.
///
/// The ordering is meaningful: `SourceOfTruth > ReferenceOnly`. Whenever two claims conflict, the
/// claim with the greater trust wins; `ReferenceOnly` material is only used as contextual fallback.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[repr(u8)]
pub enum TrustLevel {
    /// Contextual fallback: wikis, discussions, anything that may be stale or opinionated.
    #[default]
    #[serde(
        rename = "reference_only",
        alias = "ReferenceOnly",
        alias = "reference-only",
        alias = "reference"
    )]
    ReferenceOnly = 0,
    /// Authoritative baseline: the code itself, approved API definitions, the org directory.
    #[serde(
        rename = "source_of_truth",
        alias = "SourceOfTruth",
        alias = "source-of-truth",
        alias = "sot"
    )]
    SourceOfTruth = 1,
}

impl TrustLevel {
    /// Both levels, least authoritative first.
    pub const ALL: &'static [TrustLevel] = &[TrustLevel::ReferenceOnly, TrustLevel::SourceOfTruth];

    /// Stable textual name.
    pub const fn as_str(self) -> &'static str {
        match self {
            TrustLevel::ReferenceOnly => "reference_only",
            TrustLevel::SourceOfTruth => "source_of_truth",
        }
    }

    /// Stable numeric code; greater means more authoritative.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::code`].
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(TrustLevel::ReferenceOnly),
            1 => Some(TrustLevel::SourceOfTruth),
            _ => None,
        }
    }

    /// `true` for [`TrustLevel::SourceOfTruth`].
    pub const fn is_authoritative(self) -> bool {
        matches!(self, TrustLevel::SourceOfTruth)
    }
}

impl std::fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TrustLevel {
    type Err = super::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "source_of_truth" | "sourceoftruth" | "sot" | "authoritative" => {
                Ok(TrustLevel::SourceOfTruth)
            }
            "reference_only" | "referenceonly" | "reference" | "ref" => {
                Ok(TrustLevel::ReferenceOnly)
            }
            other => Err(super::Error::Invalid(format!(
                "unknown trust level `{other}` (expected `source_of_truth` or `reference_only`)"
            ))),
        }
    }
}

string_enum! {
    /// Kind of a graph node.
    pub enum NodeKind {
        // --- enterprise hierarchy -----------------------------------------------------------
        /// A portfolio: the top of the business hierarchy (e.g. "Consumer Payments").
        Portfolio = 1 => "portfolio",
        /// A business or technical domain inside a portfolio.
        Domain = 2 => "domain",
        /// A logical component/system inside a domain; groups services.
        Component = 3 => "component",
        /// A deployable service, usually one repository.
        Service = 4 => "service",
        /// A team (also: mailing list, SIG, GitHub team).
        Team = 5 => "team",
        /// A person.
        Person = 6 => "person",
        // --- code -----------------------------------------------------------------------------
        /// A file in a repository.
        File = 10 => "file",
        /// A code symbol (function, type, method, ...).
        Symbol = 11 => "symbol",
        // --- APIs -----------------------------------------------------------------------------
        /// An API definition (e.g. one OpenAPI document).
        ApiSpec = 20 => "api_spec",
        /// One operation of an API (`POST /v1/charges`).
        ApiEndpoint = 21 => "api_endpoint",
        /// An approval record/decision from an API approval portal.
        Approval = 22 => "approval",
        // --- knowledge ------------------------------------------------------------------------
        /// A wiki page (or other standalone document).
        WikiPage = 30 => "wiki_page",
        /// A comment thread (page-level, inline or review thread). Carries a status.
        Thread = 31 => "thread",
        /// One comment inside a thread.
        Comment = 32 => "comment",
        // --- work tracking --------------------------------------------------------------------
        /// A tracker issue (Jira).
        Ticket = 40 => "ticket",
        /// A pull/merge request with its review discussion.
        PullRequest = 41 => "pull_request",
    }
}

impl NodeKind {
    /// Kinds that form the enterprise hierarchy, top-down.
    pub const HIERARCHY: &'static [NodeKind] = &[
        NodeKind::Portfolio,
        NodeKind::Domain,
        NodeKind::Component,
        NodeKind::Service,
    ];

    /// `true` for kinds that represent people or groups of people.
    pub const fn is_party(self) -> bool {
        matches!(self, NodeKind::Team | NodeKind::Person)
    }

    /// `true` for kinds whose identity is established by name: two assertions of such a kind
    /// that share an identity alias describe the same real-world entity and are merged into one
    /// node, even when their keys differ (`person:asmith` from CODEOWNERS and `person:alice-smith`
    /// from a wiki byline become one person once the org directory lists both names).
    ///
    /// Every other kind is identified by its key alone; its aliases only serve name lookups.
    pub const fn merges_by_alias(self) -> bool {
        matches!(
            self,
            NodeKind::Portfolio
                | NodeKind::Domain
                | NodeKind::Component
                | NodeKind::Service
                | NodeKind::Team
                | NodeKind::Person
                | NodeKind::ApiSpec
        )
    }

    /// `true` for kinds whose aliases are matched case-sensitively and verbatim
    /// (see [`crate::base::keys::normalize_alias`]).
    pub const fn has_exact_aliases(self) -> bool {
        matches!(
            self,
            NodeKind::Symbol | NodeKind::File | NodeKind::ApiEndpoint
        )
    }
}

string_enum! {
    /// Kind of a directed graph edge, read as `from --kind--> to`.
    pub enum EdgeKind {
        // --- structure ------------------------------------------------------------------------
        /// Parent contains child: Portfolio→Domain→Component→Service→File→Symbol, nested symbols,
        /// ApiSpec→ApiEndpoint, WikiPage→child WikiPage, WikiPage|PullRequest|Ticket→Thread,
        /// Thread→Comment, epic Ticket→child Ticket, parent Team→sub Team.
        Contains = 1 => "contains",
        // --- ownership and organisation -------------------------------------------------------
        /// Team|Person owns an entity (any hierarchy level, ApiSpec, File, ...).
        Owns = 2 => "owns",
        /// Person is a member of a Team.
        MemberOf = 3 => "member_of",
        /// Person leads a Team, Portfolio or Domain.
        Leads = 4 => "leads",
        /// Person reports to Person.
        ReportsTo = 5 => "reports_to",
        // --- code -----------------------------------------------------------------------------
        /// Symbol calls Symbol.
        Calls = 10 => "calls",
        /// Symbol|File references a Symbol (type use, macro use, ...).
        References = 11 => "references",
        /// File imports a File or Service.
        Imports = 12 => "imports",
        /// Service implements an ApiSpec/ApiEndpoint; Symbol implements (handles) an ApiEndpoint.
        Implements = 13 => "implements",
        /// Service consumes an ApiSpec/ApiEndpoint.
        Consumes = 14 => "consumes",
        /// Service|Component depends on Service|Component.
        DependsOn = 15 => "depends_on",
        // --- knowledge ------------------------------------------------------------------------
        /// WikiPage (or documentation File) is the documentation of an entity.
        Documents = 20 => "documents",
        /// A text-bearing node mentions an entity. Weak evidence.
        Mentions = 21 => "mentions",
        /// Thread discusses the entity it is anchored to.
        Discusses = 22 => "discusses",
        /// Comment|WikiPage|Ticket|PullRequest was authored by a Person.
        AuthoredBy = 23 => "authored_by",
        /// Comment replies to Comment.
        ReplyTo = 24 => "reply_to",
        // --- governance and work tracking -----------------------------------------------------
        /// Approval record concerns an ApiSpec|ApiEndpoint|PullRequest.
        Approves = 30 => "approves",
        /// Approval was decided by a Person or Team.
        DecidedBy = 31 => "decided_by",
        /// `from` blocks `to` (Ticket→Ticket, Approval→ApiSpec, Thread→PullRequest, ...).
        Blocks = 32 => "blocks",
        /// Ticket|PullRequest tracks work on an entity.
        Tracks = 33 => "tracks",
        /// Ticket|PullRequest is assigned to a Person.
        AssignedTo = 34 => "assigned_to",
        /// `from` supersedes `to` (new API version, replacement page).
        Supersedes = 35 => "supersedes",
        /// Generic association.
        RelatesTo = 36 => "relates_to",
    }
}

string_enum! {
    /// Kind of a configured source.
    pub enum SourceKind {
        /// A source code repository watched on disk.
        Repo = 1 => "repo",
        /// A wiki snapshot (HTML/Markdown pages with comment threads).
        Wiki = 2 => "wiki",
        /// An API approval portal export (API definitions plus approval records).
        ApiPortal = 3 => "api_portal",
        /// A Jira export (issue JSON payloads).
        Jira = 4 => "jira",
        /// An organisation/portfolio model (portfolios, domains, components, teams, people).
        OrgModel = 5 => "org_model",
        /// Exported code-review discussions (pull requests with review threads).
        CodeReview = 6 => "code_review",
    }
}

string_enum! {
    /// Kind of an ingestion unit: the atom of ingestion, replacement and synchronisation.
    pub enum UnitKind {
        /// One source file of a repository (code or in-repo documentation).
        File = 1 => "file",
        /// Repository-level facts (the Service node, repo metadata).
        RepoMeta = 2 => "repo_meta",
        /// An ownership file (CODEOWNERS, MAINTAINERS, OWNERS).
        Ownership = 3 => "ownership",
        /// An organisation/portfolio model document.
        OrgModel = 4 => "org_model",
        /// One wiki page with its comment threads.
        WikiPage = 5 => "wiki_page",
        /// One API definition with its approval records.
        ApiSpec = 6 => "api_spec",
        /// One tracker issue.
        Ticket = 7 => "ticket",
        /// One pull request with its review threads.
        PullRequest = 8 => "pull_request",
    }
}

string_enum! {
    /// Kind of a code symbol. Stored in the `symbol_kind` attribute of Symbol nodes.
    pub enum SymbolKind {
        /// Free function.
        Function = 1 => "function",
        /// Method (function with a receiver/owner type).
        Method = 2 => "method",
        /// Struct / record.
        Struct = 3 => "struct",
        /// Enum.
        Enum = 4 => "enum",
        /// Trait.
        Trait = 5 => "trait",
        /// Interface.
        Interface = 6 => "interface",
        /// Class.
        Class = 7 => "class",
        /// Module / namespace / package.
        Module = 8 => "module",
        /// Constant.
        Constant = 9 => "constant",
        /// Global or static variable.
        Variable = 10 => "variable",
        /// Type alias / typedef.
        TypeAlias = 11 => "type_alias",
        /// Macro.
        Macro = 12 => "macro",
        /// `impl` block or equivalent extension block.
        Impl = 13 => "impl",
        /// Union.
        Union = 14 => "union",
    }
}

string_enum! {
    /// Kind of a text chunk attached to a node. Chunks are what gets full-text indexed, embedded
    /// and quoted as context.
    pub enum ChunkKind {
        /// Source code (signature plus a bounded body preview).
        Code = 1 => "code",
        /// Documentation comment / docstring of a symbol.
        Docstring = 2 => "docstring",
        /// A section of prose (wiki section, README section, RST section).
        Text = 3 => "text",
        /// The body of one comment in a thread.
        Comment = 4 => "comment",
        /// Description of a ticket, API, endpoint or entity.
        Description = 5 => "description",
        /// The rationale recorded with a decision (approval note, dismissal reason).
        Decision = 6 => "decision",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_ordering_puts_source_of_truth_on_top() {
        assert!(TrustLevel::SourceOfTruth > TrustLevel::ReferenceOnly);
        assert_eq!(
            TrustLevel::ALL.iter().max(),
            Some(&TrustLevel::SourceOfTruth)
        );
    }

    #[test]
    fn trust_parses_spec_spellings() {
        for text in ["SourceOfTruth", "source_of_truth", "source-of-truth", "SOT"] {
            assert_eq!(
                text.parse::<TrustLevel>().ok(),
                Some(TrustLevel::SourceOfTruth)
            );
        }
        for text in ["ReferenceOnly", "reference_only", "reference-only"] {
            assert_eq!(
                text.parse::<TrustLevel>().ok(),
                Some(TrustLevel::ReferenceOnly)
            );
        }
        assert!("maybe".parse::<TrustLevel>().is_err());
        let json: TrustLevel = serde_json::from_str("\"SourceOfTruth\"").expect("alias");
        assert_eq!(json, TrustLevel::SourceOfTruth);
        assert_eq!(
            serde_json::to_string(&TrustLevel::ReferenceOnly).expect("serialize"),
            "\"reference_only\""
        );
    }

    #[test]
    fn codes_and_names_round_trip() {
        for kind in NodeKind::ALL {
            assert_eq!(NodeKind::from_code(kind.code()), Some(*kind));
            assert_eq!(kind.as_str().parse::<NodeKind>().ok(), Some(*kind));
        }
        for kind in EdgeKind::ALL {
            assert_eq!(EdgeKind::from_code(kind.code()), Some(*kind));
            assert_eq!(kind.as_str().parse::<EdgeKind>().ok(), Some(*kind));
        }
        assert_eq!(NodeKind::from_code(0), None);
        assert!("nope".parse::<EdgeKind>().is_err());
    }
}
