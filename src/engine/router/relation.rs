//! Relation chains: questions that name a path through the graph.
//!
//! "Who leads the team that owns the component containing the billing service?" is not a search
//! problem; it spells out three typed hops from a named entity. The rules here read such a
//! question into a [`Chain`] of [`Hop`]s, which the query engine then follows edge by edge
//! instead of collecting whatever lies near the entity. No model is involved.
//!
//! English nests these clauses to the right, so the hop mentioned last applies first: the chain
//! is the phrases found, in reverse order of appearance.

use crate::base::{EdgeKind, IntentKind, NodeKind};
use crate::engine::graph::{Direction, TraversalProfile};

/// One typed step from the current entities to related ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hop {
    /// The person leading a team (or the team that owns the entity).
    Lead,
    /// The team that owns the entity, directly or through what contains it.
    OwnerTeam,
    /// Whoever owns the entity: a team or a person.
    Owner,
    /// The enclosing portfolio, domain or component.
    Container(NodeKind),
    /// The service implementing an API or endpoint.
    Implementer,
    /// The services consuming an API.
    Consumers,
    /// The APIs a service consumes.
    Consumed,
}

impl Hop {
    /// Node kinds this hop arrives at.
    pub fn targets(self) -> Vec<NodeKind> {
        match self {
            Hop::Lead => vec![NodeKind::Person],
            Hop::OwnerTeam => vec![NodeKind::Team],
            Hop::Owner => vec![NodeKind::Team, NodeKind::Person],
            Hop::Container(kind) => vec![kind],
            Hop::Implementer | Hop::Consumers => vec![NodeKind::Service],
            Hop::Consumed => vec![NodeKind::ApiSpec],
        }
    }

    /// `true` when every related entity answers (all consumers), `false` when only the nearest
    /// does (the enclosing domain).
    pub const fn collects_all(self) -> bool {
        matches!(self, Hop::Consumers | Hop::Consumed)
    }

    /// The edges this hop may follow.
    pub fn profile(self) -> TraversalProfile {
        use Direction::{Forward, Reverse};
        // An endpoint or API reaches the organisation through the service implementing it:
        // endpoint <-contains- API <-implements- service. `Contains` is climbed freely.
        let climb = || {
            TraversalProfile::new()
                .allow(EdgeKind::Contains, Reverse, 1.0)
                .allow_into(EdgeKind::Implements, Reverse, 1.0, &[NodeKind::Service])
        };
        match self {
            Hop::Lead => TraversalProfile::new()
                .allow_into(EdgeKind::Leads, Reverse, 0.5, &[NodeKind::Person])
                .allow_into(EdgeKind::Leads, Forward, 0.5, &[NodeKind::Person])
                .allow_into(EdgeKind::Owns, Reverse, 1.0, &[NodeKind::Team])
                .allow_into(
                    EdgeKind::Contains,
                    Reverse,
                    1.5,
                    &[
                        NodeKind::Service,
                        NodeKind::Component,
                        NodeKind::Domain,
                        NodeKind::Portfolio,
                    ],
                ),
            Hop::OwnerTeam => climb().allow_into(EdgeKind::Owns, Reverse, 0.5, &[NodeKind::Team]),
            Hop::Owner => TraversalProfile::ownership(),
            Hop::Container(_) => climb(),
            Hop::Implementer => TraversalProfile::new()
                .allow_into(EdgeKind::Contains, Reverse, 1.0, &[NodeKind::ApiSpec])
                .allow_into(EdgeKind::Implements, Reverse, 1.0, &[NodeKind::Service]),
            Hop::Consumers => TraversalProfile::new()
                .allow_into(EdgeKind::Consumes, Reverse, 1.0, &[NodeKind::Service])
                .allow_into(EdgeKind::Contains, Reverse, 0.1, &[NodeKind::ApiSpec])
                .allow_into(EdgeKind::Implements, Forward, 0.1, &[NodeKind::ApiSpec]),
            Hop::Consumed => TraversalProfile::new().allow_into(
                EdgeKind::Consumes,
                Forward,
                1.0,
                &[NodeKind::ApiSpec],
            ),
        }
    }

    /// What the hop's result is called in an answer ("Lead of X: ...").
    pub const fn label(self) -> &'static str {
        match self {
            Hop::Lead => "Lead",
            Hop::OwnerTeam => "Owning team",
            Hop::Owner => "Owner",
            Hop::Container(NodeKind::Portfolio) => "Portfolio",
            Hop::Container(NodeKind::Domain) => "Domain",
            Hop::Container(_) => "Component",
            Hop::Implementer => "Implementing service",
            Hop::Consumers => "Consumers",
            Hop::Consumed => "Consumed APIs",
        }
    }
}

/// Hops in the order they are followed from the named entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chain {
    /// The hops; never empty.
    pub hops: Vec<Hop>,
}

impl Chain {
    /// Kinds of node that answer the question.
    pub fn targets(&self) -> Vec<NodeKind> {
        self.hops.last().map_or_else(Vec::new, |hop| hop.targets())
    }

    /// The intent such a question is filed under.
    pub fn intent(&self) -> IntentKind {
        match self.hops.last() {
            Some(Hop::Implementer | Hop::Consumers | Hop::Consumed) => IntentKind::Dependency,
            _ => IntentKind::Ownership,
        }
    }

    /// What the answer is called.
    pub fn label(&self) -> &'static str {
        self.hops.last().map_or("Answer", |hop| hop.label())
    }
}

const fn container(kind: NodeKind) -> Hop {
    Hop::Container(kind)
}

/// Phrases and the hop each one names. Matching is on the lower-cased question, padded with
/// spaces and with punctuation removed; longer phrases win over shorter ones they overlap.
const PHRASES: &[(&str, Hop)] = &[
    (" who leads ", Hop::Lead),
    (" who is leading ", Hop::Lead),
    (" who heads ", Hop::Lead),
    (" who is the lead ", Hop::Lead),
    (" who is the leader ", Hop::Lead),
    (" who is the manager ", Hop::Lead),
    (" lead of ", Hop::Lead),
    (" leader of ", Hop::Lead),
    (" leads the ", Hop::Lead),
    (" team lead ", Hop::Lead),
    (" team that owns ", Hop::OwnerTeam),
    (" team which owns ", Hop::OwnerTeam),
    (" team who owns ", Hop::OwnerTeam),
    (" team owning ", Hop::OwnerTeam),
    (" team that maintains ", Hop::OwnerTeam),
    (" team responsible for ", Hop::OwnerTeam),
    (" team behind ", Hop::OwnerTeam),
    (" which team owns ", Hop::OwnerTeam),
    (" what team owns ", Hop::OwnerTeam),
    (" which team maintains ", Hop::OwnerTeam),
    (" owning team ", Hop::OwnerTeam),
    (" who owns ", Hop::Owner),
    (" who maintains ", Hop::Owner),
    (" who is responsible for ", Hop::Owner),
    (" owner of ", Hop::Owner),
    (" owners of ", Hop::Owner),
    (" which component ", container(NodeKind::Component)),
    (" what component ", container(NodeKind::Component)),
    (" component containing ", container(NodeKind::Component)),
    (" component that contains ", container(NodeKind::Component)),
    (" component which contains ", container(NodeKind::Component)),
    (" component that includes ", container(NodeKind::Component)),
    (" component of ", container(NodeKind::Component)),
    (" component for ", container(NodeKind::Component)),
    (" component behind ", container(NodeKind::Component)),
    (" which domain ", container(NodeKind::Domain)),
    (" what domain ", container(NodeKind::Domain)),
    (" domain containing ", container(NodeKind::Domain)),
    (" domain that contains ", container(NodeKind::Domain)),
    (" domain that owns ", container(NodeKind::Domain)),
    (" domain owning ", container(NodeKind::Domain)),
    (" domain of ", container(NodeKind::Domain)),
    (" domain for ", container(NodeKind::Domain)),
    (" domain behind ", container(NodeKind::Domain)),
    (" which portfolio ", container(NodeKind::Portfolio)),
    (" what portfolio ", container(NodeKind::Portfolio)),
    (" portfolio containing ", container(NodeKind::Portfolio)),
    (" portfolio that contains ", container(NodeKind::Portfolio)),
    (" portfolio that owns ", container(NodeKind::Portfolio)),
    (" portfolio of ", container(NodeKind::Portfolio)),
    (" portfolio for ", container(NodeKind::Portfolio)),
    (" code that implements ", Hop::Implementer),
    (" code implementing ", Hop::Implementer),
    (" service that implements ", Hop::Implementer),
    (" service which implements ", Hop::Implementer),
    (" service implementing ", Hop::Implementer),
    (" service that serves ", Hop::Implementer),
    (" service that exposes ", Hop::Implementer),
    (" service behind ", Hop::Implementer),
    (" which service implements ", Hop::Implementer),
    (" what implements ", Hop::Implementer),
    (" who implements ", Hop::Implementer),
    (" which apis does ", Hop::Consumed),
    (" what apis does ", Hop::Consumed),
    (" which apis do ", Hop::Consumed),
    (" apis consumed by ", Hop::Consumed),
    (" apis used by ", Hop::Consumed),
    (" consume ", Hop::Consumers),
    (" consumes ", Hop::Consumers),
    (" consuming ", Hop::Consumers),
    (" consumers of ", Hop::Consumers),
    (" consumer of ", Hop::Consumers),
    (" clients of ", Hop::Consumers),
    (" callers of ", Hop::Consumers),
    (" which services use ", Hop::Consumers),
    (" what services use ", Hop::Consumers),
    (" services that use ", Hop::Consumers),
    (" services using ", Hop::Consumers),
    (" services depend on ", Hop::Consumers),
    (" services that depend on ", Hop::Consumers),
    (" who uses ", Hop::Consumers),
    (" what uses ", Hop::Consumers),
];

/// The question as matched: lower case, words separated by single spaces, padded. Characters
/// that occur inside names (`-`, `_`, `/`, `.`, `{`, `}`, `#`, `:`) are kept.
fn matchable(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 2);
    out.push(' ');
    for word in query.split(|ch: char| {
        !(ch.is_alphanumeric() || matches!(ch, '-' | '_' | '/' | '.' | '{' | '}' | '#' | ':'))
    }) {
        let word = word.trim_matches(['.', ':']);
        if !word.is_empty() {
            out.push_str(&word.to_lowercase());
            out.push(' ');
        }
    }
    out
}

/// The question without its quoted spans: quoted text is something being referred to (a title,
/// a sentence from a page), not part of the question's own grammar.
fn unquoted(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    let mut open: Option<char> = None;
    for ch in query.chars() {
        match (open, ch) {
            (None, '"' | '`' | '\u{201c}') => {
                open = Some(if ch == '\u{201c}' { '\u{201d}' } else { ch });
            }
            (Some(close), _) if ch == close => {
                open = None;
                out.push(' ');
            }
            (None, _) => out.push(ch),
            (Some(_), _) => {}
        }
    }
    out
}

/// Reads a relation chain out of a question. `None` when the question names no relation, or
/// only plain ownership ("Who owns X?"), which the ownership intent already answers together
/// with conflict resolution.
pub fn parse(query: &str) -> Option<Chain> {
    let text = matchable(&unquoted(query));
    let mut found: Vec<(usize, usize, Hop)> = Vec::new();
    for (phrase, hop) in PHRASES {
        let mut from = 0;
        while let Some(at) = text[from..].find(phrase) {
            let start = from + at;
            found.push((start, start + phrase.len(), *hop));
            from = start + phrase.len() - 1;
        }
    }
    // Longest first, so "team that owns" beats an overlapping "who owns"-style match; a match
    // may share only its boundary space with a neighbour.
    found.sort_by_key(|(start, end, _)| (std::cmp::Reverse(end - start), *start));
    let mut kept: Vec<(usize, usize, Hop)> = Vec::new();
    for (start, end, hop) in found {
        if kept.iter().all(|(s, e, _)| end <= s + 1 || start + 1 >= *e) {
            kept.push((start, end, hop));
        }
    }
    kept.sort_by_key(|(start, _, _)| *start);
    let mut hops: Vec<Hop> = kept.into_iter().rev().map(|(_, _, hop)| hop).collect();
    hops.dedup();
    // "Which APIs does X consume?" asks what X consumes, not who consumes X.
    if hops.contains(&Hop::Consumed) {
        hops.retain(|hop| *hop != Hop::Consumers);
    }
    if hops.is_empty() || hops == [Hop::Owner] {
        return None;
    }
    Some(Chain { hops })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hops(query: &str) -> Vec<Hop> {
        parse(query).map(|chain| chain.hops).unwrap_or_default()
    }

    #[test]
    fn nested_clauses_are_followed_from_the_entity_outwards() {
        assert_eq!(
            hops("Who leads the team that owns the component containing the billing-api service?"),
            [
                Hop::Container(NodeKind::Component),
                Hop::OwnerTeam,
                Hop::Lead
            ]
        );
        assert_eq!(
            hops("Which domain owns the code that implements GET /v1/charges?"),
            [Hop::Implementer, Hop::Container(NodeKind::Domain)]
        );
        assert_eq!(
            hops("Who is the lead of the team behind the ledger service?"),
            [Hop::OwnerTeam, Hop::Lead]
        );
    }

    #[test]
    fn single_relations_are_chains_of_one() {
        assert_eq!(
            hops("Which portfolio does the billing-api service belong to?"),
            [Hop::Container(NodeKind::Portfolio)]
        );
        assert_eq!(
            hops("Which services consume the charges API?"),
            [Hop::Consumers]
        );
        assert_eq!(
            hops("List the consumers of the charges API."),
            [Hop::Consumers]
        );
        assert_eq!(
            hops("Which APIs does checkout-web consume?"),
            [Hop::Consumed]
        );
        assert_eq!(
            hops("Which team owns the fraud-scoring service?"),
            [Hop::OwnerTeam]
        );
    }

    #[test]
    fn plain_ownership_and_unrelated_questions_are_not_chains() {
        assert_eq!(parse("Who owns the billing-api service?"), None);
        assert_eq!(parse("Who is responsible for src/ledger.rs?"), None);
        assert_eq!(parse("Why is the charges API blocked?"), None);
        assert_eq!(parse("Where is charge_card defined?"), None);
        assert_eq!(parse("How do I rotate the signing keys?"), None);
    }

    #[test]
    fn quoted_text_is_not_read_as_a_relation() {
        assert_eq!(
            parse(
                "Why was the suggestion on \"Escalate to the owning team if it lasts\" dismissed?"
            ),
            None
        );
        assert_eq!(
            hops("Who leads the team that owns `who owns what`?"),
            [Hop::OwnerTeam, Hop::Lead]
        );
    }

    #[test]
    fn chains_know_their_targets_and_intent() {
        let lead = parse("Who leads the team that owns billing-api?").unwrap();
        assert_eq!(lead.targets(), [NodeKind::Person]);
        assert_eq!(lead.intent(), IntentKind::Ownership);
        assert_eq!(lead.label(), "Lead");
        let consumers = parse("Which services consume the charges API?").unwrap();
        assert_eq!(consumers.targets(), [NodeKind::Service]);
        assert_eq!(consumers.intent(), IntentKind::Dependency);
        assert!(consumers.hops[0].collects_all());
    }

    #[test]
    fn every_hop_has_a_usable_profile() {
        for (_, hop) in PHRASES {
            assert!(!hop.profile().is_empty(), "{hop:?}");
            assert!(!hop.targets().is_empty(), "{hop:?}");
        }
    }
}
