//! People and teams as sources name them.
//!
//! Every source identifies people differently: a CODEOWNERS file by handle (`@asmith`), a tracker
//! by display name and e-mail, a wiki byline by display name only. This module turns whatever a
//! source has into a node assertion whose key and aliases let the store recognise the same person
//! or team across sources (see [`NodeKind::merges_by_alias`]).

use crate::base::{Lookup, NodeAssertion, NodeKey, NodeKind, attrs, keys};

/// What a source knows about a person.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PersonIdentity {
    /// Display name ("Alice Smith").
    pub name: Option<String>,
    /// Account handle without the leading `@` ("asmith").
    pub handle: Option<String>,
    /// E-mail address.
    pub email: Option<String>,
}

fn non_empty(text: Option<&str>) -> Option<String> {
    text.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

impl PersonIdentity {
    /// Builds an identity from optional parts; blank parts are treated as absent.
    pub fn new(name: Option<&str>, handle: Option<&str>, email: Option<&str>) -> Self {
        Self {
            name: non_empty(name),
            handle: non_empty(handle).map(|handle| handle.trim_start_matches('@').to_owned()),
            email: non_empty(email).map(|email| email.to_ascii_lowercase()),
        }
    }

    /// Parses the ways people are written in text files:
    /// `Alice Smith <alice@example.com>`, `@asmith`, `alice@example.com`, `Alice Smith`.
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        if let (Some(open), Some(close)) = (text.find('<'), text.rfind('>'))
            && open < close
        {
            let inside = text[open + 1..close].trim();
            let name = text[..open].trim().trim_matches('"');
            return if inside.contains('@') && !inside.starts_with('@') {
                Self::new(Some(name), None, Some(inside))
            } else {
                Self::new(Some(name), Some(inside), None)
            };
        }
        if let Some(handle) = text.strip_prefix('@') {
            return Self::new(None, Some(handle), None);
        }
        if text.contains('@') && !text.contains(char::is_whitespace) {
            return Self::new(None, None, Some(text));
        }
        Self::new(Some(text), None, None)
    }

    fn email_local_part(&self) -> Option<&str> {
        self.email
            .as_deref()
            .and_then(|email| email.split('@').next())
            .filter(|local| !local.is_empty())
    }

    /// `true` when nothing identifies the person.
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.handle.is_none() && self.email.is_none()
    }

    /// The node key: derived from the handle when known, else the e-mail local part, else the name.
    pub fn key(&self) -> Option<NodeKey> {
        let identifier = self
            .handle
            .as_deref()
            .or_else(|| self.email_local_part())
            .or(self.name.as_deref())?;
        let key = keys::person(identifier);
        (key.as_str() != "person:").then_some(key)
    }

    /// The best display name available.
    pub fn display_name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or(self.handle.as_deref())
            .or(self.email.as_deref())
    }

    /// A Person assertion carrying every known identifier as an alias, so that other sources
    /// naming the same person by a different identifier resolve to the same node.
    pub fn assertion(&self) -> Option<NodeAssertion> {
        let key = self.key()?;
        let mut node = NodeAssertion::new(NodeKind::Person, key, self.display_name()?);
        for alias in [
            self.handle.as_deref(),
            self.email.as_deref(),
            self.email_local_part(),
            self.name.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            node = node.alias(alias);
        }
        Some(node.attr_opt(attrs::EMAIL, self.email.clone()))
    }

    /// A lookup for this person, for sources that only want to reference (not assert) them.
    pub fn lookup(&self) -> Option<Lookup> {
        let text = self
            .handle
            .as_deref()
            .or(self.email.as_deref())
            .or(self.name.as_deref())?;
        Some(Lookup::new(&[NodeKind::Person], text))
    }
}

/// `true` for references that denote a group rather than an individual: `@org/team` handles and
/// mailing-list style addresses are teams.
pub fn is_team_reference(reference: &str) -> bool {
    let reference = reference.trim();
    reference.starts_with('@') && reference.contains('/')
}

/// A Team assertion from a team reference (`@acme/payments-platform`, `Payments Platform`).
///
/// For `@org/team` handles the key is derived from the team part alone, and both the full handle
/// and the bare team name are registered as aliases.
pub fn team_assertion(reference: &str) -> Option<NodeAssertion> {
    let reference = reference.trim();
    let bare = reference.trim_start_matches('@');
    if bare.is_empty() {
        return None;
    }
    let short = bare.rsplit('/').next().unwrap_or(bare);
    let key = keys::team(short);
    if key.as_str() == "team:" {
        return None;
    }
    let mut node = NodeAssertion::new(NodeKind::Team, key, short).alias(short);
    if short != bare {
        node = node.alias(bare);
    }
    Some(node)
}

/// A Person or Team assertion from an owner token as found in ownership files: `@org/team` is a
/// team, anything else (`@user`, `user@example.com`, `Alice Smith <a@example.com>`) is a person.
pub fn party_assertion(reference: &str) -> Option<NodeAssertion> {
    if is_team_reference(reference) {
        team_assertion(reference)
    } else {
        PersonIdentity::parse(reference).assertion()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_spellings() {
        let full = PersonIdentity::parse("Alice Smith <Alice.Smith@Example.com>");
        assert_eq!(full.name.as_deref(), Some("Alice Smith"));
        assert_eq!(full.email.as_deref(), Some("alice.smith@example.com"));
        assert_eq!(full.handle, None);

        assert_eq!(
            PersonIdentity::parse("@asmith").handle.as_deref(),
            Some("asmith")
        );
        assert_eq!(
            PersonIdentity::parse("bob@example.com").email.as_deref(),
            Some("bob@example.com")
        );
        assert_eq!(
            PersonIdentity::parse("Bob Jones").name.as_deref(),
            Some("Bob Jones")
        );
        assert_eq!(
            PersonIdentity::parse("Carol <@carol>").handle.as_deref(),
            Some("carol")
        );
        assert!(PersonIdentity::parse("   ").is_empty());
        assert!(PersonIdentity::parse("").assertion().is_none());
    }

    #[test]
    fn key_prefers_handle_then_email_then_name() {
        let all = PersonIdentity::new(Some("Alice Smith"), Some("@asmith"), Some("alice@x.io"));
        assert_eq!(all.key().expect("key").as_str(), "person:asmith");
        let no_handle = PersonIdentity::new(Some("Alice Smith"), None, Some("alice.smith@x.io"));
        assert_eq!(no_handle.key().expect("key").as_str(), "person:alice-smith");
        let name_only = PersonIdentity::new(Some("Alice Smith"), None, None);
        assert_eq!(name_only.key().expect("key").as_str(), "person:alice-smith");
    }

    #[test]
    fn assertion_carries_every_identifier_as_alias() {
        let person = PersonIdentity::new(Some("Alice Smith"), Some("asmith"), Some("alice@x.io"))
            .assertion()
            .expect("assertion");
        assert_eq!(person.kind, NodeKind::Person);
        assert_eq!(person.name, "Alice Smith");
        for alias in ["asmith", "alice@x.io", "alice", "Alice Smith"] {
            assert!(
                person.aliases.iter().any(|a| a == alias),
                "missing alias {alias}"
            );
        }
        assert_eq!(
            person.attrs.get(attrs::EMAIL),
            Some(&serde_json::Value::from("alice@x.io"))
        );
    }

    #[test]
    fn teams_are_keyed_by_their_short_name() {
        let team = team_assertion("@acme/payments-platform").expect("team");
        assert_eq!(team.key.as_str(), "team:payments-platform");
        assert_eq!(
            team.aliases,
            vec!["payments-platform", "acme/payments-platform"]
        );
        assert_eq!(
            team_assertion("Payments Platform")
                .expect("team")
                .key
                .as_str(),
            "team:payments-platform"
        );
        assert!(team_assertion("@").is_none());
    }

    #[test]
    fn owner_tokens_dispatch_to_person_or_team() {
        assert_eq!(
            party_assertion("@acme/sre").expect("team").kind,
            NodeKind::Team
        );
        assert_eq!(
            party_assertion("@bob").expect("person").kind,
            NodeKind::Person
        );
        assert_eq!(
            party_assertion("bob@example.com")
                .expect("person")
                .key
                .as_str(),
            "person:bob"
        );
    }
}
