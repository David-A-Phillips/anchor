//! Canonical natural keys and name normalisation.
//!
//! Every producer (code parser, wiki/Jira/API ingestion, org model, sync) must build keys through
//! these functions so that independent sources agree on the identity of an entity. Keys have the
//! shape `<prefix>:<body>`; the prefix determines the [`NodeKind`].
//!
//! | Kind | Key | Example |
//! |------|-----|---------|
//! | Portfolio | `portfolio:<slug>` | `portfolio:consumer-payments` |
//! | Domain | `domain:<slug>` | `domain:billing` |
//! | Component | `component:<slug>` | `component:ledger` |
//! | Service | `service:<slug>` | `service:payments-api` |
//! | Team | `team:<slug>` | `team:payments-platform` |
//! | Person | `person:<slug>` | `person:alice-smith` |
//! | File | `file:<source>:<path>` | `file:payments-api:src/lib.rs` |
//! | Symbol | `symbol:<source>:<path>#<qualified name>` | `symbol:payments-api:src/lib.rs#Ledger::charge` |
//! | ApiSpec | `api:<slug>` | `api:payments-api` |
//! | ApiEndpoint | `endpoint:<api slug>:<METHOD> <route>` | `endpoint:payments-api:POST /v1/charges` |
//! | Approval | `approval:<source>:<id>` | `approval:portal:APR-17` |
//! | WikiPage | `wiki:<source>:<id>` | `wiki:eng-wiki:payments-runbook` |
//! | Thread | `thread:<source>:<id>` | `thread:eng-wiki:t-204` |
//! | Comment | `comment:<source>:<id>` | `comment:eng-wiki:c-991` |
//! | Ticket | `ticket:<KEY>` | `ticket:PAY-123` |
//! | PullRequest | `pr:<repo slug>#<number>` | `pr:payments-api#482` |

use super::ids::{NodeKey, SourceId, UnitKey};
use super::kinds::{NodeKind, UnitKind};

const PREFIXES: &[(&str, NodeKind)] = &[
    ("portfolio", NodeKind::Portfolio),
    ("domain", NodeKind::Domain),
    ("component", NodeKind::Component),
    ("service", NodeKind::Service),
    ("team", NodeKind::Team),
    ("person", NodeKind::Person),
    ("file", NodeKind::File),
    ("symbol", NodeKind::Symbol),
    ("commit", NodeKind::Commit),
    ("api", NodeKind::ApiSpec),
    ("endpoint", NodeKind::ApiEndpoint),
    ("approval", NodeKind::Approval),
    ("wiki", NodeKind::WikiPage),
    ("thread", NodeKind::Thread),
    ("comment", NodeKind::Comment),
    ("ticket", NodeKind::Ticket),
    ("pr", NodeKind::PullRequest),
];

/// Key prefix used for nodes of `kind`.
pub fn prefix(kind: NodeKind) -> &'static str {
    PREFIXES
        .iter()
        .find(|(_, candidate)| *candidate == kind)
        .map(|(prefix, _)| *prefix)
        .unwrap_or("node")
}

/// The [`NodeKind`] a key belongs to, derived from its prefix.
pub fn kind_of(key: &NodeKey) -> Option<NodeKind> {
    let (prefix, _) = key.as_str().split_once(':')?;
    PREFIXES
        .iter()
        .find(|(candidate, _)| *candidate == prefix)
        .map(|(_, kind)| *kind)
}

/// Lowercases and reduces `text` to alphanumeric runs joined by single hyphens.
///
/// `"Payments API"`, `"payments_api"` and `"@payments-api"` all become `payments-api`; a display
/// name `"Alice Smith"` and a handle `"alice.smith"` both become `alice-smith`.
pub fn slugify(text: &str) -> String {
    let mut slug = String::with_capacity(text.len());
    let mut pending_separator = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            pending_separator = false;
            slug.extend(ch.to_lowercase());
        } else {
            pending_separator = true;
        }
    }
    slug
}

/// Normalises an alias for the entity-resolution index.
///
/// Symbols, files and API endpoints are matched verbatim (identifiers are case-sensitive); every
/// other kind is matched on its [`slugify`]d form. Both the index writer and every lookup must use
/// this function.
pub fn normalize_alias(kind: NodeKind, text: &str) -> String {
    if kind.has_exact_aliases() {
        text.trim().to_owned()
    } else {
        slugify(text)
    }
}

/// Normalises a repository-relative path: forward slashes, no leading `./` or `/`.
pub fn normalize_path(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let mut trimmed = unified.as_str();
    while let Some(rest) = trimmed.strip_prefix("./") {
        trimmed = rest;
    }
    trimmed.trim_start_matches('/').to_owned()
}

fn slug_key(kind: NodeKind, name: &str) -> NodeKey {
    NodeKey::new(format!("{}:{}", prefix(kind), slugify(name)))
}

/// Key of a [`NodeKind::Portfolio`].
pub fn portfolio(name: &str) -> NodeKey {
    slug_key(NodeKind::Portfolio, name)
}

/// Key of a [`NodeKind::Domain`].
pub fn domain(name: &str) -> NodeKey {
    slug_key(NodeKind::Domain, name)
}

/// Key of a [`NodeKind::Component`].
pub fn component(name: &str) -> NodeKey {
    slug_key(NodeKind::Component, name)
}

/// Key of a [`NodeKind::Service`].
pub fn service(name: &str) -> NodeKey {
    slug_key(NodeKind::Service, name)
}

/// Key of a [`NodeKind::Team`].
pub fn team(name: &str) -> NodeKey {
    slug_key(NodeKind::Team, name)
}

/// Key of a [`NodeKind::Person`], from whatever identifier the source has (handle, e-mail local
/// part or display name). Sources that know several identifiers list the others as aliases.
pub fn person(identifier: &str) -> NodeKey {
    slug_key(NodeKind::Person, identifier)
}

/// Key of a [`NodeKind::File`].
pub fn file(source: &SourceId, path: &str) -> NodeKey {
    NodeKey::new(format!("file:{}:{}", source, normalize_path(path)))
}

/// Key of a [`NodeKind::Symbol`]. `qualified_name` is the container chain joined with `::`
/// (e.g. `Ledger::charge`), with a `~N` suffix when a file defines the same name more than once.
pub fn symbol(source: &SourceId, path: &str, qualified_name: &str) -> NodeKey {
    NodeKey::new(format!(
        "symbol:{}:{}#{}",
        source,
        normalize_path(path),
        qualified_name
    ))
}

/// Key of a [`NodeKind::ApiSpec`].
pub fn api_spec(name: &str) -> NodeKey {
    slug_key(NodeKind::ApiSpec, name)
}

/// Key of a [`NodeKind::ApiEndpoint`]; the method is upper-cased, the route kept verbatim.
pub fn api_endpoint(api_name: &str, method: &str, route: &str) -> NodeKey {
    NodeKey::new(format!(
        "endpoint:{}:{} {}",
        slugify(api_name),
        method.trim().to_ascii_uppercase(),
        route.trim()
    ))
}

/// Key of a [`NodeKind::Approval`].
pub fn approval(source: &SourceId, id: &str) -> NodeKey {
    NodeKey::new(format!("approval:{}:{}", source, id.trim()))
}

/// Key of a [`NodeKind::WikiPage`].
pub fn wiki_page(source: &SourceId, id: &str) -> NodeKey {
    NodeKey::new(format!("wiki:{}:{}", source, id.trim()))
}

/// Key of a [`NodeKind::Thread`].
pub fn thread(source: &SourceId, id: &str) -> NodeKey {
    NodeKey::new(format!("thread:{}:{}", source, id.trim()))
}

/// Key of a [`NodeKind::Comment`].
pub fn comment(source: &SourceId, id: &str) -> NodeKey {
    NodeKey::new(format!("comment:{}:{}", source, id.trim()))
}

/// Key of a [`NodeKind::Ticket`]; tracker keys are upper-cased (`pay-123` → `ticket:PAY-123`).
pub fn ticket(issue_key: &str) -> NodeKey {
    NodeKey::new(format!("ticket:{}", issue_key.trim().to_ascii_uppercase()))
}

/// Key of a [`NodeKind::Commit`]: the full hash within its repository source.
pub fn commit(source: &SourceId, sha: &str) -> NodeKey {
    NodeKey::new(format!(
        "commit:{}:{}",
        source,
        sha.trim().to_ascii_lowercase()
    ))
}

/// Key of a [`NodeKind::PullRequest`].
pub fn pull_request(repo: &str, number: u64) -> NodeKey {
    NodeKey::new(format!("pr:{}#{}", slugify(repo), number))
}

/// Key of an ingestion unit: `<source>/<unit kind>/<local id>`.
pub fn unit(source: &SourceId, kind: UnitKind, local_id: &str) -> UnitKey {
    UnitKey::new(format!("{}/{}/{}", source, kind.as_str(), local_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_unifies_spellings() {
        for text in [
            "Payments API",
            "payments_api",
            "@payments-api",
            "  payments.api  ",
        ] {
            assert_eq!(slugify(text), "payments-api", "{text}");
        }
        assert_eq!(slugify("Alice Smith"), slugify("alice.smith"));
        assert_eq!(slugify("---"), "");
        assert_eq!(slugify("Zoë Ångström"), "zoë-ångström");
    }

    #[test]
    fn aliases_are_exact_for_code_and_slugged_otherwise() {
        assert_eq!(
            normalize_alias(NodeKind::Symbol, " Ledger::charge "),
            "Ledger::charge"
        );
        assert_eq!(
            normalize_alias(NodeKind::Team, "Payments Platform"),
            "payments-platform"
        );
        assert_eq!(normalize_alias(NodeKind::Person, "@asmith"), "asmith");
    }

    #[test]
    fn keys_have_expected_shapes_and_kinds() {
        let source = SourceId::new("payments-api");
        let cases = [
            (
                portfolio("Consumer Payments"),
                "portfolio:consumer-payments",
                NodeKind::Portfolio,
            ),
            (
                service("Payments API"),
                "service:payments-api",
                NodeKind::Service,
            ),
            (
                person("Alice Smith"),
                "person:alice-smith",
                NodeKind::Person,
            ),
            (
                file(&source, "./src\\lib.rs"),
                "file:payments-api:src/lib.rs",
                NodeKind::File,
            ),
            (
                symbol(&source, "src/lib.rs", "Ledger::charge"),
                "symbol:payments-api:src/lib.rs#Ledger::charge",
                NodeKind::Symbol,
            ),
            (
                api_endpoint("Payments API", "post", "/v1/charges"),
                "endpoint:payments-api:POST /v1/charges",
                NodeKind::ApiEndpoint,
            ),
            (ticket("pay-123"), "ticket:PAY-123", NodeKind::Ticket),
            (
                pull_request("Payments API", 482),
                "pr:payments-api#482",
                NodeKind::PullRequest,
            ),
        ];
        for (key, expected, kind) in cases {
            assert_eq!(key.as_str(), expected);
            assert_eq!(kind_of(&key), Some(kind));
        }
        assert_eq!(kind_of(&NodeKey::new("mystery:thing")), None);
        assert_eq!(kind_of(&NodeKey::new("no-prefix")), None);
    }

    #[test]
    fn every_kind_has_a_distinct_prefix() {
        let mut seen = std::collections::BTreeSet::new();
        for kind in NodeKind::ALL {
            let prefix = prefix(*kind);
            assert_ne!(prefix, "node", "{kind} has no prefix");
            assert!(seen.insert(prefix), "duplicate prefix {prefix}");
        }
    }

    #[test]
    fn unit_keys_are_namespaced_by_source() {
        let key = unit(&SourceId::new("eng-wiki"), UnitKind::WikiPage, "runbook");
        assert_eq!(key.as_str(), "eng-wiki/wiki_page/runbook");
    }
}
