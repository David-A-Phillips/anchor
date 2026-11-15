//! Word lists. Everything the generator names comes from here; nothing is read from disk.

pub(crate) const FIRST_NAMES: &[&str] = &[
    "Alice", "Bob", "Carol", "David", "Elena", "Farid", "Grace", "Hiro", "Ines", "Jamal", "Katya",
    "Liam", "Mei", "Nadia", "Omar", "Priya", "Quinn", "Rosa", "Sven", "Tara", "Umar", "Vera",
    "Wen", "Ximena", "Yusuf", "Zoe", "Anders", "Bianca", "Chidi", "Dara",
];

pub(crate) const LAST_NAMES: &[&str] = &[
    "Smith", "Jones", "White", "Nguyen", "Garcia", "Okafor", "Ivanov", "Tanaka", "Silva", "Khan",
    "Muller", "Rossi", "Haddad", "Larsen", "Costa", "Patel", "Novak", "Kim", "Dubois", "Berg",
    "Moreau", "Fischer", "Lopez", "Brown", "Singh", "Jensen", "Popov", "Reyes", "Walsh", "Yilmaz",
];

pub(crate) const ROLES: &[&str] = &[
    "Staff Engineer",
    "Senior Engineer",
    "Engineer",
    "SRE",
    "Tech Lead",
    "Product Engineer",
];

pub(crate) const PORTFOLIO_NAMES: &[&str] = &[
    "Consumer Payments",
    "Merchant Services",
    "Lending",
    "Wealth Management",
    "Risk And Compliance",
    "Corporate Banking",
    "Insurance",
    "Treasury",
    "Customer Experience",
    "Data Platform",
    "Developer Tools",
    "Logistics",
];

pub(crate) const DOMAIN_NAMES: &[&str] = &[
    "Billing",
    "Ledger",
    "Onboarding",
    "Identity",
    "Fraud",
    "Notifications",
    "Reporting",
    "Settlement",
    "Pricing",
    "Cards",
    "Disputes",
    "Scoring",
    "Statements",
    "Payouts",
    "Treasury Ops",
    "Auditing",
    "Search",
    "Messaging",
    "Catalog",
    "Fulfilment",
];

pub(crate) const COMPONENT_NAMES: &[&str] = &[
    "Ledger Core",
    "Invoice Engine",
    "Rate Cards",
    "Session Manager",
    "Rules Engine",
    "Alert Router",
    "Export Pipeline",
    "Clearing House",
    "Quote Builder",
    "Card Vault",
    "Case Manager",
    "Score Model",
    "Statement Renderer",
    "Payout Scheduler",
    "Audit Trail",
    "Index Builder",
    "Message Bus",
    "Product Feed",
    "Order Tracker",
    "Tax Calculator",
    "Address Book",
    "Consent Registry",
    "Limit Controller",
    "Reconciler",
    "Token Service",
    "Webhook Hub",
    "Currency Desk",
    "Policy Store",
    "Risk Gateway",
    "Customer Graph",
];

pub(crate) const TEAM_AREAS: &[&str] = &[
    "Payments",
    "Billing",
    "Identity",
    "Risk",
    "Lending",
    "Ledger",
    "Messaging",
    "Data",
    "Platform",
    "Search",
    "Cards",
    "Treasury",
];

pub(crate) const TEAM_KINDS: &[&str] = &[
    "Platform",
    "Reliability",
    "Core",
    "Integrations",
    "Experience",
    "Foundations",
];

/// Leading part of service names. Few on purpose: `payments-api` next to `payments-gateway` is the
/// kind of near-duplicate a retriever must tell apart.
pub(crate) const SERVICE_STEMS: &[&str] = &[
    "payments",
    "ledger",
    "billing",
    "invoices",
    "identity",
    "sessions",
    "fraud",
    "alerts",
    "reports",
    "settlement",
    "pricing",
    "cards",
    "disputes",
    "scoring",
    "statements",
    "payouts",
    "audit",
    "search",
    "messages",
    "catalog",
    "orders",
    "tax",
    "contacts",
    "consent",
    "limits",
    "recon",
    "tokens",
    "webhooks",
    "currency",
    "policy",
];

pub(crate) const SERVICE_ROLES: &[&str] = &[
    "api",
    "gateway",
    "service",
    "worker",
    "web",
    "sync",
    "engine",
    "store",
    "scheduler",
    "adapter",
    "bridge",
    "proxy",
];

pub(crate) const VERBS: &[&str] = &[
    "compute",
    "validate",
    "load",
    "store",
    "render",
    "reconcile",
    "route",
    "normalize",
    "audit",
    "publish",
    "resolve",
    "encode",
    "schedule",
    "verify",
    "export",
    "refresh",
];

pub(crate) const NOUNS: &[&str] = &[
    "balance",
    "invoice",
    "session",
    "profile",
    "ledger",
    "payment",
    "report",
    "quota",
    "account",
    "token",
    "policy",
    "limit",
    "refund",
    "statement",
    "mandate",
    "receipt",
];

pub(crate) const RESOURCES: &[&str] = &[
    "charges", "refunds", "accounts", "invoices", "sessions", "tokens", "limits", "payouts",
    "reports", "policies", "mandates", "receipts",
];

pub(crate) const WIKI_TOPICS: &[&str] = &[
    "Incident response",
    "Capacity planning",
    "Release process",
    "Data retention",
    "On-call handover",
    "Schema migration",
    "Rate limiting",
    "Disaster recovery",
    "Key rotation",
    "Feature flags",
];

pub(crate) const BLOCK_REASONS: &[&str] = &[
    "waiting for the security review of the new token format",
    "waiting for the vendor to ship the sandbox credentials",
    "waiting for the schema migration to finish in staging",
    "waiting for legal sign-off on the data retention change",
    "waiting for the upstream team to publish the new contract",
    "waiting for the capacity test to be scheduled",
];

pub(crate) const REJECTION_REASONS: &[&str] = &[
    "Card numbers must not appear in query strings.",
    "The error model leaks internal stack traces to callers.",
    "Pagination is missing on list operations, which risks unbounded responses.",
    "Authentication relies on a shared static secret instead of per-client tokens.",
    "The data classification of the response is undeclared.",
    "Idempotency keys are not supported on mutating operations.",
];

pub(crate) const REVIEW_BLOCK_REASONS: &[&str] = &[
    "This loop retries forever when the upstream returns 503; add a bounded backoff.",
    "The new column is not covered by the migration rollback.",
    "Secrets are logged at info level here; mask them before merging.",
    "Please add a test for the empty-input branch before this lands.",
    "This changes the public error type without a deprecation period.",
];

pub(crate) const DISMISSAL_REASONS: &[&str] = &[
    "Dismissed because the concern was addressed by the follow-up commit.",
    "Dismissed because the reviewer left the team and the thread is obsolete.",
    "Dismissed after the design was agreed in the architecture review.",
    "Dismissed because the requested change is out of scope for this pull request.",
];

pub(crate) const WIKI_DISMISS_REASONS: &[&str] = &[
    "Superseded by the idempotency keys decision; the proposed retry loop is no longer needed.",
    "Out of scope for this runbook; tracked as a separate design note.",
    "Rejected because the suggested alert threshold would page the team every night.",
    "Declined: the proposal duplicates the circuit breaker already in the gateway.",
];

pub(crate) const WIKI_RESOLVE_NOTES: &[&str] = &[
    "Fixed in the latest revision.",
    "Thanks, the diagram has been updated.",
    "Typo corrected.",
];
