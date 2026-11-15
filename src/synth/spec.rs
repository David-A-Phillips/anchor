//! The generator's input: [`SynthSpec`], its presets and builder.

use serde::{Deserialize, Serialize};

/// A language the generator can write source code in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynthLanguage {
    /// Rust.
    Rust,
    /// Python.
    Python,
    /// TypeScript.
    TypeScript,
    /// Go.
    Go,
    /// Java.
    Java,
    /// C.
    C,
}

impl SynthLanguage {
    /// Every language, in a fixed order.
    pub const ALL: [SynthLanguage; 6] = [
        SynthLanguage::Rust,
        SynthLanguage::Python,
        SynthLanguage::TypeScript,
        SynthLanguage::Go,
        SynthLanguage::Java,
        SynthLanguage::C,
    ];
}

/// Relative frequency of a language among services.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageWeight {
    /// The language.
    pub language: SynthLanguage,
    /// Relative weight (zero disables the language).
    pub weight: u32,
}

/// Probabilities of the interesting cases. Each is applied as a quota over the services (or
/// tickets, pull requests, ...) so that even tiny datasets contain at least one of each case.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaseRates {
    /// Services whose ownership is declared in a MAINTAINERS file.
    pub maintainers_file: f64,
    /// Services whose ownership is declared in OWNERS files.
    pub owners_file: f64,
    /// Services without any direct owner: the owner is inherited from the component.
    pub inherited_ownership: f64,
    /// Services whose ownership is contradicted by a stale reference-level claim.
    pub conflict: f64,
    /// Services outside the org model that only a reference-level source assigns an owner to.
    pub reference_only_owner: f64,
    /// Services directly owned by a team other than their component's team.
    pub cross_team_owner: f64,
    /// Average number of cross-service calls per service.
    pub calls_per_service: f64,
    /// Share of APIs with a blocking rejection (whole API).
    pub rejected_api: f64,
    /// Share of APIs with a blocking changes-requested approval on one operation.
    pub changes_requested_operation: f64,
    /// Share of APIs whose approval is still pending.
    pub pending_api: f64,
    /// Share of wiki comment threads that are dismissed.
    pub dismissed_thread: f64,
    /// Share of tickets that are blocked.
    pub blocked_ticket: f64,
    /// Share of pull requests with an unresolved blocking review.
    pub blocking_review: f64,
    /// Share of pull requests whose review was dismissed.
    pub dismissed_review: f64,
}

impl Default for CaseRates {
    fn default() -> Self {
        Self {
            maintainers_file: 0.12,
            owners_file: 0.12,
            inherited_ownership: 0.15,
            conflict: 0.12,
            reference_only_owner: 0.06,
            cross_team_owner: 0.2,
            calls_per_service: 1.3,
            rejected_api: 0.2,
            changes_requested_operation: 0.2,
            pending_api: 0.15,
            dismissed_thread: 0.3,
            blocked_ticket: 0.2,
            blocking_review: 0.25,
            dismissed_review: 0.25,
        }
    }
}

/// Named sizes, see [`SynthSpec::preset`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    /// 8 repositories: milliseconds, for unit tests.
    Tiny,
    /// 24 repositories.
    Small,
    /// 100 repositories.
    Medium,
    /// 1000 repositories.
    Large,
}

/// Everything that determines a generated dataset. The same spec always yields byte-identical
/// output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SynthSpec {
    /// Seed of every pseudo-random choice.
    pub seed: u64,
    /// Number of portfolios.
    pub portfolios: usize,
    /// Domains per portfolio.
    pub domains_per_portfolio: usize,
    /// Components per domain.
    pub components_per_domain: usize,
    /// Services (repositories) per component.
    pub services_per_component: usize,
    /// When set, only this many repositories are generated (the hierarchy is filled in order, so
    /// the last component may be partly filled). Lets a preset be scaled to an exact count.
    pub repository_limit: Option<usize>,
    /// Source files per service (at least 1).
    pub files_per_service: usize,
    /// Functions per file (at least 1).
    pub symbols_per_file: usize,
    /// Teams (at least 2).
    pub teams: usize,
    /// People (raised to the number of teams when lower).
    pub people: usize,
    /// Wiki pages (ownership pages, runbooks and design notes).
    pub wiki_pages: usize,
    /// Jira issues.
    pub tickets: usize,
    /// API records (at most one per service).
    pub apis: usize,
    /// Pull requests.
    pub pull_requests: usize,
    /// Language mix of the services.
    pub languages: Vec<LanguageWeight>,
    /// Rates of the interesting cases.
    pub rates: CaseRates,
}

impl Default for SynthSpec {
    fn default() -> Self {
        Self::preset(Preset::Tiny)
    }
}

fn default_languages() -> Vec<LanguageWeight> {
    [
        (SynthLanguage::Rust, 3),
        (SynthLanguage::Python, 3),
        (SynthLanguage::TypeScript, 3),
        (SynthLanguage::Go, 2),
        (SynthLanguage::Java, 2),
        (SynthLanguage::C, 1),
    ]
    .into_iter()
    .map(|(language, weight)| LanguageWeight { language, weight })
    .collect()
}

impl SynthSpec {
    /// The spec of a named size.
    pub fn preset(preset: Preset) -> Self {
        let base = Self {
            seed: 1,
            portfolios: 1,
            domains_per_portfolio: 2,
            components_per_domain: 2,
            services_per_component: 2,
            repository_limit: None,
            files_per_service: 3,
            symbols_per_file: 3,
            teams: 4,
            people: 14,
            wiki_pages: 9,
            tickets: 14,
            apis: 4,
            pull_requests: 8,
            languages: default_languages(),
            rates: CaseRates::default(),
        };
        match preset {
            Preset::Tiny => base,
            Preset::Small => Self::default_scaled(base, 24),
            Preset::Medium => Self::default_scaled(base, 100),
            Preset::Large => Self::default_scaled(base, 1000),
        }
    }

    fn default_scaled(base: Self, repositories: usize) -> Self {
        Self {
            files_per_service: 4,
            symbols_per_file: 4,
            ..base
        }
        .with_repositories(repositories)
    }

    /// Scales the hierarchy and every entity count to exactly `repositories` repositories
    /// (10, 100, 1000, ...). The other fields keep their values.
    pub fn with_repositories(mut self, repositories: usize) -> Self {
        let n = repositories.max(1);
        let domains = if n <= 12 { 2 } else { 3 };
        let components = if n <= 12 {
            2
        } else if n <= 100 {
            3
        } else {
            4
        };
        let per_portfolio = domains * components * 5;
        self.portfolios = n.div_ceil(per_portfolio).clamp(1, 40);
        self.domains_per_portfolio = domains;
        self.components_per_domain = components;
        let slots = self.portfolios * domains * components;
        self.services_per_component = n.div_ceil(slots).max(1);
        self.repository_limit = Some(n);
        self.teams = (n / 6).max(4);
        self.people = self.teams * 4 + 2;
        self.wiki_pages = n / 2 + 8;
        self.tickets = n + 12;
        self.apis = (n / 2).max(4).min(n);
        self.pull_requests = n / 2 + 6;
        self
    }

    /// Sets the seed.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Sets the language mix.
    pub fn languages(mut self, languages: Vec<LanguageWeight>) -> Self {
        self.languages = languages;
        self
    }

    /// Sets files per service and functions per file.
    pub fn code_size(mut self, files_per_service: usize, symbols_per_file: usize) -> Self {
        self.files_per_service = files_per_service;
        self.symbols_per_file = symbols_per_file;
        self
    }

    /// Sets the number of wiki pages, tickets and pull requests.
    pub fn discussion_counts(
        mut self,
        wiki_pages: usize,
        tickets: usize,
        pull_requests: usize,
    ) -> Self {
        self.wiki_pages = wiki_pages;
        self.tickets = tickets;
        self.pull_requests = pull_requests;
        self
    }

    /// Sets the number of API records.
    pub fn apis(mut self, apis: usize) -> Self {
        self.apis = apis;
        self
    }

    /// Sets the case rates.
    pub fn rates(mut self, rates: CaseRates) -> Self {
        self.rates = rates;
        self
    }

    /// The number of repositories this spec generates.
    pub fn repositories(&self) -> usize {
        let full = self.portfolios
            * self.domains_per_portfolio
            * self.components_per_domain
            * self.services_per_component;
        self.repository_limit.map_or(full, |limit| limit.min(full))
    }
}
