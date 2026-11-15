//! Candidate entity mentions in free text.
//!
//! Wiki pages, comments, ticket descriptions and API descriptions refer to code, APIs, tickets and
//! people by name. Producers cannot know which names denote real entities, so they emit every
//! plausible candidate as a [`Lookup`]; the store's linker materialises a `Mentions` edge only when
//! a candidate resolves to exactly one node, and keeps the rest pending in case the entity appears
//! later.
//!
//! Recognised shapes and the node kinds they are looked up as:
//!
//! | Shape | Example | Kinds | Weight |
//! |-------|---------|-------|--------|
//! | Tracker key | `PAY-123` | Ticket | 0.9 |
//! | HTTP route | `POST /v1/charges` | ApiEndpoint | 0.9 |
//! | @-mention | `@asmith`, `@acme/payments` | Person, Team | 0.8 |
//! | Code span naming a path | `` `src/ledger.rs` `` | File | 0.7 |
//! | Code span | `` `Ledger::charge` `` | Symbol, ApiSpec, Service, Component | 0.7 (last segment: 0.4) |
//! | Bare path | `src/ledger.rs` | File | 0.6 |
//! | Hyphenated/snake name | `payments-api` | Service, Component, ApiSpec, Domain, Team | 0.5 |
//! | CamelCase name | `PaymentsService` | Symbol, Service, Component | 0.4 |
//!
//! Producers of the target nodes make these lookups succeed by registering matching aliases:
//! endpoints register `"<METHOD> <route>"`, files their repository-relative path, symbols their
//! simple and qualified names, tickets their key.

use std::sync::LazyLock;

use regex::Regex;

use crate::base::{EdgeAssertion, EdgeKind, Lookup, NodeKey, NodeKind};

/// Upper bound on mentions extracted from one text.
pub const MAX_MENTIONS: usize = 64;

/// A candidate reference found in text.
#[derive(Clone, Debug, PartialEq)]
pub struct Mention {
    /// What to look up.
    pub lookup: Lookup,
    /// Confidence that the text really refers to such an entity, in `(0, 1]`.
    pub weight: f32,
}

/// Prefixes that look like tracker projects but are not (`UTF-8`, `SHA-256`, `CVE-2024-1234`).
const NOT_TICKET_PREFIXES: &[&str] = &[
    "AES", "ANSI", "CVE", "COVID", "HTTP", "IEEE", "ISO", "MD", "PEP", "RFC", "SHA", "SSL", "TLS",
    "UTF", "X",
];

fn regex(pattern: &str) -> Regex {
    // Patterns are compile-time constants covered by tests; failing to compile is a programming
    // error, not a runtime condition.
    Regex::new(pattern).expect("mention pattern compiles")
}

static TICKET: LazyLock<Regex> = LazyLock::new(|| regex(r"\b([A-Z][A-Z0-9]{1,9})-(\d{1,7})\b"));
static ROUTE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"\b(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+(/[A-Za-z0-9_\-./{}:]*[A-Za-z0-9_}])")
});
static AT_MENTION: LazyLock<Regex> =
    LazyLock::new(|| regex(r"@([A-Za-z0-9][A-Za-z0-9_\-]*(?:[./][A-Za-z0-9_\-]+)*)"));
static CODE_SPAN: LazyLock<Regex> = LazyLock::new(|| regex(r"`([^`\n]{2,160})`"));
static BARE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    regex(r#"(?:^|[\s("'\[<])((?:[A-Za-z0-9_.\-]+/)+[A-Za-z0-9_.\-]+\.[A-Za-z0-9]{1,8})\b"#)
});
static COMPOUND: LazyLock<Regex> = LazyLock::new(|| regex(r"\b[a-z][a-z0-9]*(?:[-_][a-z0-9]+)+\b"));
static CAMEL: LazyLock<Regex> = LazyLock::new(|| regex(r"\b[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]+)+\b"));

const CODE_KINDS: &[NodeKind] = &[
    NodeKind::Symbol,
    NodeKind::ApiSpec,
    NodeKind::Service,
    NodeKind::Component,
];
const NAME_KINDS: &[NodeKind] = &[
    NodeKind::Service,
    NodeKind::Component,
    NodeKind::ApiSpec,
    NodeKind::Domain,
    NodeKind::Team,
];
const CAMEL_KINDS: &[NodeKind] = &[NodeKind::Symbol, NodeKind::Service, NodeKind::Component];

struct Collector {
    found: Vec<(usize, Mention)>,
}

impl Collector {
    fn push(&mut self, position: usize, kinds: &[NodeKind], text: &str, weight: f32) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let lookup = Lookup::new(kinds, text);
        match self.found.iter_mut().find(|(_, m)| m.lookup == lookup) {
            Some((_, existing)) => existing.weight = existing.weight.max(weight),
            None => self.found.push((position, Mention { lookup, weight })),
        }
    }

    fn finish(mut self) -> Vec<Mention> {
        if self.found.len() > MAX_MENTIONS {
            self.found
                .sort_by(|a, b| b.1.weight.total_cmp(&a.1.weight).then(a.0.cmp(&b.0)));
            self.found.truncate(MAX_MENTIONS);
        }
        self.found.sort_by_key(|(position, _)| *position);
        self.found.into_iter().map(|(_, mention)| mention).collect()
    }
}

fn looks_like_path(text: &str) -> bool {
    text.contains('/')
        && !text.starts_with('/')
        && !text.contains("://")
        && text
            .rsplit('/')
            .next()
            .is_some_and(|name| name.contains('.') && !name.ends_with('.'))
}

/// Reduces the contents of a code span to the name it most plausibly refers to: call arguments,
/// generic parameters and sigils are dropped. Returns `None` for snippets rather than names.
fn clean_code_span(span: &str) -> Option<&str> {
    let mut text = span.trim();
    if let Some(open) = text.find(['(', '<', '[', '{']) {
        text = &text[..open];
    }
    let text = text
        .trim()
        .trim_start_matches(['&', '*', '$', '@'])
        .trim_end_matches([';', ',', ':', '.']);
    if text.len() < 2 || text.chars().any(char::is_whitespace) {
        return None;
    }
    if !text.chars().any(char::is_alphabetic) {
        return None;
    }
    Some(text)
}

fn last_segment(name: &str) -> Option<&str> {
    let segment = name.rsplit([':', '.', '#', '>', '/']).next()?;
    (segment.len() >= 2 && segment != name).then_some(segment)
}

/// Extracts candidate mentions from `text`, deduplicated, in order of first appearance, capped at
/// [`MAX_MENTIONS`] (the highest-weighted are kept).
pub fn extract_mentions(text: &str) -> Vec<Mention> {
    let mut out = Collector { found: Vec::new() };
    if text.trim().is_empty() {
        return Vec::new();
    }

    for found in TICKET.captures_iter(text) {
        let (Some(whole), Some(prefix)) = (found.get(0), found.get(1)) else {
            continue;
        };
        if NOT_TICKET_PREFIXES.contains(&prefix.as_str()) {
            continue;
        }
        out.push(whole.start(), &[NodeKind::Ticket], whole.as_str(), 0.9);
    }

    for found in ROUTE.captures_iter(text) {
        let (Some(whole), Some(method), Some(route)) = (found.get(0), found.get(1), found.get(2))
        else {
            continue;
        };
        let endpoint = format!("{} {}", method.as_str(), route.as_str());
        out.push(whole.start(), &[NodeKind::ApiEndpoint], &endpoint, 0.9);
    }

    for found in AT_MENTION.captures_iter(text) {
        let (Some(whole), Some(handle)) = (found.get(0), found.get(1)) else {
            continue;
        };
        // An `@` glued to a preceding word is the middle of an e-mail address, not a mention.
        let glued = text[..whole.start()]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-' | '+'));
        if glued {
            continue;
        }
        let kinds: &[NodeKind] = if handle.as_str().contains('/') {
            &[NodeKind::Team, NodeKind::Person]
        } else {
            &[NodeKind::Person, NodeKind::Team]
        };
        out.push(whole.start(), kinds, handle.as_str(), 0.8);
    }

    for found in CODE_SPAN.captures_iter(text) {
        let (Some(whole), Some(span)) = (found.get(0), found.get(1)) else {
            continue;
        };
        let Some(name) = clean_code_span(span.as_str()) else {
            continue;
        };
        if looks_like_path(name) {
            out.push(whole.start(), &[NodeKind::File], name, 0.7);
            continue;
        }
        out.push(whole.start(), CODE_KINDS, name, 0.7);
        if let Some(segment) = last_segment(name) {
            out.push(whole.start(), &[NodeKind::Symbol], segment, 0.4);
        }
    }

    for found in BARE_PATH.captures_iter(text) {
        let Some(path) = found.get(1) else {
            continue;
        };
        if looks_like_path(path.as_str()) {
            out.push(path.start(), &[NodeKind::File], path.as_str(), 0.6);
        }
    }

    for found in COMPOUND.find_iter(text) {
        out.push(found.start(), NAME_KINDS, found.as_str(), 0.5);
    }

    for found in CAMEL.find_iter(text) {
        out.push(found.start(), CAMEL_KINDS, found.as_str(), 0.4);
    }

    out.finish()
}

/// `Mentions` edges from `from` to every candidate found in `text`.
pub fn mention_edges(from: &NodeKey, text: &str) -> Vec<EdgeAssertion> {
    extract_mentions(text)
        .into_iter()
        .map(|mention| {
            EdgeAssertion::new(from, EdgeKind::Mentions, mention.lookup).weight(mention.weight)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(mentions: &'a [Mention], text: &str) -> Vec<&'a Mention> {
        mentions
            .iter()
            .filter(|mention| mention.lookup.text == text)
            .collect()
    }

    fn first_kind(mentions: &[Mention], text: &str) -> Option<NodeKind> {
        find(mentions, text)
            .first()
            .and_then(|mention| mention.lookup.kinds.first().copied())
    }

    #[test]
    fn recognises_each_shape() {
        let text = "PAY-123 is blocked: `Ledger::charge()` in src/ledger.rs fails on \
                    POST /v1/charges, see `docs/runbook.md`. @asmith and @acme/payments-platform \
                    own payments-api (the PaymentsService).";
        let mentions = extract_mentions(text);

        assert_eq!(first_kind(&mentions, "PAY-123"), Some(NodeKind::Ticket));
        assert_eq!(
            first_kind(&mentions, "POST /v1/charges"),
            Some(NodeKind::ApiEndpoint)
        );
        assert_eq!(first_kind(&mentions, "asmith"), Some(NodeKind::Person));
        assert_eq!(
            first_kind(&mentions, "acme/payments-platform"),
            Some(NodeKind::Team)
        );
        assert_eq!(
            first_kind(&mentions, "Ledger::charge"),
            Some(NodeKind::Symbol)
        );
        assert_eq!(first_kind(&mentions, "charge"), Some(NodeKind::Symbol));
        assert_eq!(first_kind(&mentions, "src/ledger.rs"), Some(NodeKind::File));
        assert_eq!(
            first_kind(&mentions, "docs/runbook.md"),
            Some(NodeKind::File)
        );
        assert_eq!(
            first_kind(&mentions, "payments-api"),
            Some(NodeKind::Service)
        );
        assert_eq!(
            first_kind(&mentions, "PaymentsService"),
            Some(NodeKind::Symbol)
        );
    }

    #[test]
    fn ignores_lookalikes() {
        let mentions = extract_mentions(
            "Encode as UTF-8 and hash with SHA-256 (see CVE-2024-1234). Mail bob@example.com or \
             read https://example.com/docs/guide.html. Run `cargo test --all` then `x`.",
        );
        for absent in [
            "UTF-8",
            "SHA-256",
            "CVE-2024",
            "example.com",
            "cargo test --all",
        ] {
            assert!(
                find(&mentions, absent).is_empty(),
                "{absent} must not be a mention"
            );
        }
        assert!(
            mentions
                .iter()
                .all(|m| m.lookup.kinds != [NodeKind::Person, NodeKind::Team]),
            "an e-mail address is not an @-mention: {mentions:?}"
        );
        assert!(
            mentions
                .iter()
                .all(|m| !m.lookup.kinds.contains(&NodeKind::File)),
            "a URL is not a repository path: {mentions:?}"
        );
    }

    #[test]
    fn deduplicates_and_keeps_the_strongest_weight() {
        let mentions = extract_mentions("payments-api, payments-api and again payments-api");
        assert_eq!(find(&mentions, "payments-api").len(), 1);
        assert!(extract_mentions("   ").is_empty());
    }

    #[test]
    fn caps_the_number_of_mentions_preferring_strong_ones() {
        let mut text = String::new();
        for index in 0..200 {
            text.push_str(&format!("word-{index} "));
        }
        text.push_str("OPS-7");
        let mentions = extract_mentions(&text);
        assert_eq!(mentions.len(), MAX_MENTIONS);
        assert_eq!(find(&mentions, "OPS-7").len(), 1);
    }

    #[test]
    fn builds_weighted_mention_edges() {
        let from = NodeKey::new("wiki:eng:runbook");
        let edges = mention_edges(&from, "See PAY-9 and payments-api.");
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|edge| edge.kind == EdgeKind::Mentions));
        assert!(edges.iter().any(|edge| (edge.weight - 0.9).abs() < 1e-6));
    }
}
