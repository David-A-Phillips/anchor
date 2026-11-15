//! Organisation and portfolio models: portfolios, domains, components, services, teams, people.
//!
//! [`parse_org_model`] turns one document (YAML, JSON or TOML, chosen by file extension) into one
//! [`UnitKind::OrgModel`] unit. This is the directory that lets engineers and non-coding
//! stakeholders ask "who owns X?" at any level of the hierarchy.
//!
//! # Format
//!
//! All top-level keys are optional. Unknown keys are ignored.
//!
//! ```yaml
//! organisation: acme            # optional; prefixes team handles ("acme/payments-platform")
//! portfolios:
//!   - name: Consumer Payments
//!     description: Everything that moves money for consumers.
//!     lead: asmith              # a person
//!     owner: payments-platform  # a team or person; a string or a list of strings
//!     aliases: [CP]
//!     domains:
//!       - name: Billing
//!         lead: Bob Jones
//!         owner: "@acme/billing"
//!         components:
//!           - name: Ledger
//!             owner: payments-platform
//!             description: Double-entry ledger.
//!             services:
//!               - payments-api                    # just a name
//!               - name: ledger-writer
//!                 repo: acme/ledger-writer
//!                 owner: bjones
//!                 tier: 1
//!                 url: https://ledger.example.com
//!                 description: Writes postings.
//! teams:
//!   - name: Payments Platform
//!     handle: payments-platform   # or "@acme/payments-platform"
//!     email: payments@example.com
//!     lead: asmith
//!     parent: Engineering
//!     description: Owns the payment rails.
//!     members:
//!       - asmith
//!       - { name: Carol White, handle: cwhite, email: carol@example.com, role: SRE }
//! people:
//!   - name: Alice Smith
//!     handle: asmith
//!     email: alice.smith@example.com
//!     aliases: [alice]
//!     role: Engineering Manager
//!     reports_to: bjones
//!     team: payments-platform     # a string or a list of strings
//! ```
//!
//! The hierarchy may also be written flat, each entity naming its parent:
//!
//! ```yaml
//! domains:    [ { name: Billing, portfolio: Consumer Payments } ]
//! components: [ { name: Ledger, domain: Billing } ]
//! services:   [ { name: payments-api, component: Ledger, owner: payments-platform } ]
//! ```
//!
//! Both forms can be mixed, and every level accepts `name`, `description`, `owner`, `lead`,
//! `aliases` and `url`; services also `repo` and `tier`.
//!
//! # References
//!
//! `owner`, `lead`, `reports_to`, `team`, `parent`, `members` and the flat-form parent fields are
//! references by name. A reference is resolved against the entities the document defines, by any
//! of their aliases (so `asmith`, `alice.smith@example.com` and `Alice Smith` all denote the same
//! person). An `owner` prefers a team over a person of the same name; `@org/team` is always a
//! team. A reference to something the document does not define is still emitted as an edge to the
//! key the name would have (it stays pending until some unit asserts that entity) and reported as
//! a warning.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::base::{
    ChunkKind, EdgeAssertion, EdgeKind, Error, NodeAssertion, NodeKey, NodeKind, Result, SourceId,
    Unit, UnitKind, attrs, keys,
};
use crate::engine::ingest::party::{self, PersonIdentity};
use crate::engine::ingest::{IngestOutput, IngestWarning};

/// Service attribute: the repository hosting the service.
pub const ATTR_REPO: &str = "repo";
/// Service attribute: the criticality tier.
pub const ATTR_TIER: &str = "tier";

/// YAML and JSON documents often carry explicit nulls for empty sections.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// One reference or a list of them.
#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
enum Refs {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl Refs {
    fn to_vec(&self) -> Vec<String> {
        match self {
            Refs::None => Vec::new(),
            Refs::One(one) => vec![one.clone()],
            Refs::Many(many) => many.clone(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OrgDoc {
    #[serde(alias = "organization", alias = "org")]
    organisation: Option<String>,
    #[serde(deserialize_with = "null_default")]
    portfolios: Vec<EntityEntry>,
    #[serde(deserialize_with = "null_default")]
    domains: Vec<EntityEntry>,
    #[serde(deserialize_with = "null_default")]
    components: Vec<EntityEntry>,
    #[serde(deserialize_with = "null_default")]
    services: Vec<EntityEntry>,
    #[serde(deserialize_with = "null_default")]
    teams: Vec<TeamDef>,
    #[serde(deserialize_with = "null_default")]
    people: Vec<PersonDef>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EntityEntry {
    Name(String),
    Full(Box<EntityDef>),
}

/// A portfolio, domain, component or service. One shape serves all four levels: which children
/// and parent fields are meaningful depends on the level, and unused ones are simply absent.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EntityDef {
    name: Option<String>,
    description: Option<String>,
    lead: Option<String>,
    #[serde(deserialize_with = "null_default")]
    owner: Refs,
    #[serde(deserialize_with = "null_default")]
    aliases: Vec<String>,
    url: Option<String>,
    repo: Option<String>,
    #[serde(deserialize_with = "null_default")]
    tier: Value,
    portfolio: Option<String>,
    domain: Option<String>,
    component: Option<String>,
    #[serde(deserialize_with = "null_default")]
    domains: Vec<EntityEntry>,
    #[serde(deserialize_with = "null_default")]
    components: Vec<EntityEntry>,
    #[serde(deserialize_with = "null_default")]
    services: Vec<EntityEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TeamDef {
    name: Option<String>,
    handle: Option<String>,
    email: Option<String>,
    lead: Option<String>,
    parent: Option<String>,
    description: Option<String>,
    #[serde(deserialize_with = "null_default")]
    aliases: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    members: Vec<MemberEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MemberEntry {
    Name(String),
    Full(PersonDef),
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PersonDef {
    name: Option<String>,
    handle: Option<String>,
    email: Option<String>,
    #[serde(deserialize_with = "null_default")]
    aliases: Vec<String>,
    role: Option<String>,
    reports_to: Option<String>,
    #[serde(deserialize_with = "null_default")]
    team: Refs,
}

/// A reference to resolve once every definition of the document is known.
struct Pending {
    edge: EdgeKind,
    /// The defined entity the reference was written on.
    subject: NodeKey,
    /// Kind the target is expected to have.
    target: NodeKind,
    text: String,
    /// `true`: `target --edge--> subject`; `false`: `subject --edge--> target`.
    target_is_tail: bool,
    location: String,
}

struct Builder {
    unit: Unit,
    /// `(kind, normalised alias)` of every defined entity.
    index: HashMap<(NodeKind, String), NodeKey>,
    pending: Vec<Pending>,
    warnings: Vec<IngestWarning>,
    organisation: Option<String>,
}

fn hierarchy_key(kind: NodeKind, name: &str) -> NodeKey {
    match kind {
        NodeKind::Portfolio => keys::portfolio(name),
        NodeKind::Domain => keys::domain(name),
        NodeKind::Component => keys::component(name),
        NodeKind::Team => keys::team(name),
        _ => keys::service(name),
    }
}

impl Builder {
    fn warn(&mut self, location: &str, message: String) {
        self.warnings.push(IngestWarning::new(location, message));
    }

    /// Asserts a node and makes it referable by its key body, name and aliases.
    fn define(&mut self, node: NodeAssertion) -> NodeKey {
        let kind = node.kind;
        let key = node.key.clone();
        let body = key.as_str().split_once(':').map_or("", |(_, body)| body);
        let names = [body, node.name.as_str()]
            .into_iter()
            .chain(node.aliases.iter().map(String::as_str));
        for name in names {
            let alias = keys::normalize_alias(kind, name);
            if !alias.is_empty() {
                self.index
                    .entry((kind, alias))
                    .or_insert_with(|| key.clone());
            }
        }
        self.unit.add_node(node)
    }

    fn refer(
        &mut self,
        edge: EdgeKind,
        subject: &NodeKey,
        target: NodeKind,
        text: &str,
        target_is_tail: bool,
        location: &str,
    ) {
        self.pending.push(Pending {
            edge,
            subject: subject.clone(),
            target,
            text: text.to_owned(),
            target_is_tail,
            location: location.to_owned(),
        });
    }

    fn entity(&mut self, kind: NodeKind, entry: EntityEntry, parent: Option<&NodeKey>, at: &str) {
        let def = match entry {
            EntityEntry::Name(name) => EntityDef {
                name: Some(name),
                ..EntityDef::default()
            },
            EntityEntry::Full(def) => *def,
        };
        let name = def.name.as_deref().map(str::trim).unwrap_or_default();
        if keys::slugify(name).is_empty() {
            self.warn(at, format!("skipped a {kind:?} without a usable name"));
            return;
        }
        let location = format!("{at}/{name}");
        let mut node = NodeAssertion::new(kind, hierarchy_key(kind, name), name)
            .alias(name)
            .attr_opt(attrs::URL, def.url)
            .attr_opt(ATTR_REPO, def.repo);
        for alias in def.aliases {
            node = node.alias(alias);
        }
        if !def.tier.is_null() {
            node = node.attr(ATTR_TIER, def.tier);
        }
        let key = self.define(node);
        if let Some(description) = def.description {
            self.unit
                .add_chunk(&key, ChunkKind::Description, description.trim());
        }
        if let Some(parent) = parent {
            self.unit
                .add_edge(EdgeAssertion::new(parent, EdgeKind::Contains, &key));
        }
        for (parent_kind, parent_name) in [
            (NodeKind::Portfolio, def.portfolio),
            (NodeKind::Domain, def.domain),
            (NodeKind::Component, def.component),
        ] {
            if let Some(parent_name) = parent_name {
                self.refer(
                    EdgeKind::Contains,
                    &key,
                    parent_kind,
                    &parent_name,
                    true,
                    &location,
                );
            }
        }
        for owner in def.owner.to_vec() {
            self.refer(
                EdgeKind::Owns,
                &key,
                NodeKind::Team,
                &owner,
                true,
                &location,
            );
        }
        if let Some(lead) = def.lead {
            self.refer(
                EdgeKind::Leads,
                &key,
                NodeKind::Person,
                &lead,
                true,
                &location,
            );
        }
        for (child_kind, children) in [
            (NodeKind::Domain, def.domains),
            (NodeKind::Component, def.components),
            (NodeKind::Service, def.services),
        ] {
            for child in children {
                self.entity(child_kind, child, Some(&key), &location);
            }
        }
    }

    /// Defines a person. A person already defined under another identifier (matched by handle,
    /// e-mail or name) keeps their key and gains the new aliases.
    fn person(&mut self, def: PersonDef, at: &str) -> Option<NodeKey> {
        let identity = PersonIdentity::new(
            def.name.as_deref(),
            def.handle.as_deref(),
            def.email.as_deref(),
        );
        let Some(mut node) = identity.assertion() else {
            self.warn(
                at,
                "skipped a person without name, handle or e-mail".to_owned(),
            );
            return None;
        };
        let known = [&identity.handle, &identity.email, &identity.name]
            .into_iter()
            .flatten()
            .find_map(|id| {
                let alias = keys::normalize_alias(NodeKind::Person, id);
                self.index.get(&(NodeKind::Person, alias)).cloned()
            });
        if let Some(known) = known {
            node.key = known;
        }
        for alias in def.aliases {
            node = node.alias(alias);
        }
        let location = format!("{at}/{}", node.name);
        let key = self.define(node.attr_opt(attrs::ROLE, def.role));
        if let Some(manager) = def.reports_to {
            self.refer(
                EdgeKind::ReportsTo,
                &key,
                NodeKind::Person,
                &manager,
                false,
                &location,
            );
        }
        for team in def.team.to_vec() {
            self.refer(
                EdgeKind::MemberOf,
                &key,
                NodeKind::Team,
                &team,
                false,
                &location,
            );
        }
        Some(key)
    }

    fn team(&mut self, def: TeamDef) {
        let handle = def
            .handle
            .as_deref()
            .map(|handle| handle.trim().trim_start_matches('@'))
            .filter(|handle| !handle.is_empty());
        let short = handle.map(|handle| handle.rsplit('/').next().unwrap_or(handle));
        let name = def
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .or(short)
            .unwrap_or_default();
        // Keyed by handle when there is one, so the key matches what CODEOWNERS-style
        // `@org/team` references produce.
        let key = keys::team(short.unwrap_or(name));
        if key.as_str() == "team:" {
            self.warn(
                "teams",
                "skipped a team without a usable name or handle".to_owned(),
            );
            return;
        }
        let location = format!("teams/{name}");
        let mut node = NodeAssertion::new(NodeKind::Team, key, name).alias(name);
        if let (Some(handle), Some(short)) = (handle, short) {
            node = node.alias(short);
            if handle != short {
                node = node.alias(handle);
            } else if let Some(organisation) = &self.organisation {
                node = node.alias(format!("{organisation}/{short}"));
            }
        }
        let email = def
            .email
            .map(|email| email.trim().to_ascii_lowercase())
            .filter(|email| !email.is_empty());
        if let Some(email) = &email {
            node = node.alias(email.clone());
        }
        for alias in def.aliases {
            node = node.alias(alias);
        }
        let key = self.define(node.attr_opt(attrs::EMAIL, email));
        if let Some(description) = def.description {
            self.unit
                .add_chunk(&key, ChunkKind::Description, description.trim());
        }
        if let Some(lead) = def.lead {
            self.refer(
                EdgeKind::Leads,
                &key,
                NodeKind::Person,
                &lead,
                true,
                &location,
            );
        }
        if let Some(parent) = def.parent {
            self.refer(
                EdgeKind::Contains,
                &key,
                NodeKind::Team,
                &parent,
                true,
                &location,
            );
        }
        for member in def.members {
            match member {
                MemberEntry::Name(text) => {
                    self.refer(
                        EdgeKind::MemberOf,
                        &key,
                        NodeKind::Person,
                        &text,
                        true,
                        &location,
                    );
                }
                MemberEntry::Full(mut person) => {
                    // The role of an inline member describes the membership, not the person.
                    let role = person.role.take();
                    if let Some(member) = self.person(person, &location) {
                        let mut edge = EdgeAssertion::new(&member, EdgeKind::MemberOf, &key);
                        if let Some(role) = role {
                            edge = edge.attr(attrs::ROLE, role);
                        }
                        self.unit.add_edge(edge);
                    }
                }
            }
        }
    }

    fn lookup(&self, kind: NodeKind, text: &str) -> Option<NodeKey> {
        let alias = keys::normalize_alias(kind, text);
        self.index.get(&(kind, alias)).cloned()
    }

    /// Resolves a reference to a key and whether the document defines the target.
    fn resolve(&self, target: NodeKind, text: &str) -> Option<(NodeKey, bool)> {
        let text = text.trim();
        if !target.is_party() {
            return match self.lookup(target, text) {
                Some(key) => Some((key, true)),
                None => {
                    let key = hierarchy_key(target, text);
                    (!key.as_str().ends_with(':')).then_some((key, false))
                }
            };
        }
        let team_reference = party::is_team_reference(text);
        let bare = text.trim_start_matches('@');
        let short = bare.rsplit('/').next().unwrap_or(bare);
        let identity = PersonIdentity::parse(text);
        let person_names: Vec<&str> = [&identity.handle, &identity.email, &identity.name]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        let order: &[NodeKind] = if team_reference {
            &[NodeKind::Team]
        } else if target == NodeKind::Team {
            &[NodeKind::Team, NodeKind::Person]
        } else {
            &[NodeKind::Person, NodeKind::Team]
        };
        for &kind in order {
            let names: &[&str] = if kind == NodeKind::Team {
                &[bare, short]
            } else {
                &person_names
            };
            if let Some(key) = names.iter().find_map(|name| self.lookup(kind, name)) {
                return Some((key, true));
            }
        }
        // Undefined: `@org/team` is a team, `@handle` and e-mail addresses are people, a bare
        // name is whatever the field expects.
        let kind = if team_reference {
            NodeKind::Team
        } else if text.starts_with('@') || identity.email.is_some() {
            NodeKind::Person
        } else {
            target
        };
        let key = if kind == NodeKind::Team {
            Some(keys::team(short)).filter(|key| key.as_str() != "team:")
        } else {
            identity.key()
        };
        key.map(|key| (key, false))
    }

    fn link(&mut self) {
        for pending in std::mem::take(&mut self.pending) {
            let Some((target, defined)) = self.resolve(pending.target, &pending.text) else {
                self.warn(
                    &pending.location,
                    format!("ignored unusable reference `{}`", pending.text),
                );
                continue;
            };
            if !defined {
                self.warn(
                    &pending.location,
                    format!(
                        "`{}` is not defined in this document; referenced as {target}",
                        pending.text
                    ),
                );
            }
            let edge = if pending.target_is_tail {
                EdgeAssertion::new(target, pending.edge, pending.subject)
            } else {
                EdgeAssertion::new(pending.subject, pending.edge, target)
            };
            self.unit.add_edge(edge);
        }
    }
}

fn parse_document(rel_path: &str, text: &str) -> Result<OrgDoc> {
    let extension = rel_path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let malformed = |err: &dyn std::fmt::Display| Error::parse(format!("{rel_path}: {err}"));
    // Going through a JSON value gives the three syntaxes identical typing rules.
    let value: Value = match extension.as_str() {
        "yaml" | "yml" => serde_yaml_ng::from_str(text).map_err(|err| malformed(&err))?,
        "json" => serde_json::from_str(text).map_err(|err| malformed(&err))?,
        "toml" => toml::from_str(text).map_err(|err| malformed(&err))?,
        _ => {
            return Err(Error::invalid(format!(
                "{rel_path}: unsupported organisation model format (expected .yaml, .yml, .json or .toml)"
            )));
        }
    };
    if value.is_null() {
        return Ok(OrgDoc::default());
    }
    serde_json::from_value(value).map_err(|err| malformed(&err))
}

/// Parses an organisation model document into one [`UnitKind::OrgModel`] unit (key
/// `keys::unit(source, UnitKind::OrgModel, rel_path)`), in canonical form.
///
/// The syntax is chosen by the extension of `rel_path` (`.yaml`, `.yml`, `.json`, `.toml`); see
/// the [module documentation](self) for the format. The unit asserts Portfolio, Domain,
/// Component, Service, Team and Person nodes with their aliases, `Contains` edges down the
/// hierarchy and from parent teams, `Owns`, `Leads`, `MemberOf` and `ReportsTo` edges, and
/// Description chunks.
///
/// Errors: [`Error::Invalid`] for an unsupported extension, [`Error::Parse`] for a document that
/// is syntactically malformed or does not have the documented shape. Entities without a usable
/// name and references to undefined entities are warnings, not errors.
pub fn parse_org_model(source: &SourceId, rel_path: &str, text: &str) -> Result<IngestOutput> {
    let doc = parse_document(rel_path, text)?;
    let path = keys::normalize_path(rel_path);
    let mut builder = Builder {
        unit: Unit::new(
            keys::unit(source, UnitKind::OrgModel, &path),
            source.clone(),
            UnitKind::OrgModel,
        ),
        index: HashMap::new(),
        pending: Vec::new(),
        warnings: Vec::new(),
        organisation: doc
            .organisation
            .map(|org| org.trim().trim_start_matches('@').to_owned())
            .filter(|org| !org.is_empty()),
    };
    // People first, so inline team members recognise them; references are linked last, once
    // every definition is known, so the order of sections in the document does not matter.
    for person in doc.people {
        builder.person(person, "people");
    }
    for team in doc.teams {
        builder.team(team);
    }
    for (kind, section, entries) in [
        (NodeKind::Portfolio, "portfolios", doc.portfolios),
        (NodeKind::Domain, "domains", doc.domains),
        (NodeKind::Component, "components", doc.components),
        (NodeKind::Service, "services", doc.services),
    ] {
        for entry in entries {
            builder.entity(kind, entry, None, section);
        }
    }
    builder.link();
    Ok(IngestOutput {
        units: vec![builder.unit.finish()],
        warnings: builder.warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NESTED_YAML: &str = r#"
organisation: acme
portfolios:
  - name: Consumer Payments
    description: Everything that moves money for consumers.
    lead: asmith
    owner: payments-platform
    domains:
      - name: Billing
        lead: Bob Jones
        owner: "@acme/billing"
        components:
          - name: Ledger
            owner: Payments Platform
            description: Double-entry ledger.
            services:
              - payments-api
              - name: ledger-writer
                repo: acme/ledger-writer
                owner: bjones
                tier: 1
                description: Writes postings.
teams:
  - name: Payments Platform
    handle: payments-platform
    email: Payments@Example.com
    lead: asmith
    parent: Engineering
    members:
      - asmith
      - { name: Carol White, handle: cwhite, email: carol@example.com, role: SRE }
  - name: Engineering
people:
  - name: Alice Smith
    handle: asmith
    email: alice.smith@example.com
    aliases: [alice]
    role: Engineering Manager
    reports_to: bjones
    team: payments-platform
  - name: Bob Jones
    handle: bjones
"#;

    fn parse(path: &str, text: &str) -> IngestOutput {
        parse_org_model(&SourceId::from("org"), path, text).unwrap()
    }

    /// Edges rendered as `from -kind-> to`, with lookups (never produced here) failing loudly.
    fn edges(unit: &Unit) -> Vec<String> {
        unit.edges
            .iter()
            .map(|edge| {
                let end = |node: &crate::base::NodeRef| {
                    serde_json::to_value(node).unwrap()["key"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                };
                format!(
                    "{} -{}-> {}",
                    end(&edge.from),
                    edge.kind.as_str(),
                    end(&edge.to)
                )
            })
            .collect()
    }

    fn node<'a>(unit: &'a Unit, key: &str) -> &'a NodeAssertion {
        unit.nodes
            .iter()
            .find(|node| node.key.as_str() == key)
            .unwrap_or_else(|| panic!("no node {key}"))
    }

    #[test]
    fn nested_yaml_builds_the_hierarchy_and_the_directory() {
        let output = parse("org.yaml", NESTED_YAML);
        let unit = &output.units[0];
        assert_eq!(unit.kind, UnitKind::OrgModel);
        assert_eq!(
            unit.key,
            keys::unit(&SourceId::from("org"), UnitKind::OrgModel, "org.yaml")
        );
        let keys_: Vec<_> = unit.nodes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(
            keys_,
            [
                "component:ledger",
                "domain:billing",
                "person:asmith",
                "person:bjones",
                "person:cwhite",
                "portfolio:consumer-payments",
                "service:ledger-writer",
                "service:payments-api",
                "team:engineering",
                "team:payments-platform",
            ]
        );
        assert_eq!(
            edges(unit),
            [
                "component:ledger -contains-> service:ledger-writer",
                "component:ledger -contains-> service:payments-api",
                "domain:billing -contains-> component:ledger",
                "person:asmith -member_of-> team:payments-platform",
                "person:asmith -leads-> portfolio:consumer-payments",
                "person:asmith -leads-> team:payments-platform",
                "person:asmith -reports_to-> person:bjones",
                "person:bjones -owns-> service:ledger-writer",
                "person:bjones -leads-> domain:billing",
                "person:cwhite -member_of-> team:payments-platform",
                "portfolio:consumer-payments -contains-> domain:billing",
                "team:billing -owns-> domain:billing",
                "team:engineering -contains-> team:payments-platform",
                "team:payments-platform -owns-> component:ledger",
                "team:payments-platform -owns-> portfolio:consumer-payments",
            ]
        );
        // The only dangling reference is the undefined billing team.
        assert_eq!(output.warnings.len(), 1, "{:?}", output.warnings);
        assert_eq!(
            output.warnings[0].location,
            "portfolios/Consumer Payments/Billing"
        );
        assert!(output.warnings[0].message.contains("team:billing"));

        let service = node(unit, "service:ledger-writer");
        assert_eq!(service.attrs[ATTR_REPO], "acme/ledger-writer");
        assert_eq!(service.attrs[ATTR_TIER], 1);
        let carol = unit
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::MemberOf && e.attrs.contains_key(attrs::ROLE))
            .unwrap();
        assert_eq!(carol.attrs[attrs::ROLE], "SRE");
        let chunks: Vec<_> = unit
            .chunks
            .iter()
            .map(|c| (c.node.as_str(), c.kind, c.text.as_str()))
            .collect();
        assert_eq!(
            chunks,
            [
                (
                    "component:ledger",
                    ChunkKind::Description,
                    "Double-entry ledger."
                ),
                (
                    "portfolio:consumer-payments",
                    ChunkKind::Description,
                    "Everything that moves money for consumers."
                ),
                (
                    "service:ledger-writer",
                    ChunkKind::Description,
                    "Writes postings."
                ),
            ]
        );
    }

    #[test]
    fn aliases_cover_every_identifier() {
        let output = parse("org.yaml", NESTED_YAML);
        let unit = &output.units[0];
        let alice = node(unit, "person:asmith");
        assert_eq!(alice.name, "Alice Smith");
        assert_eq!(
            alice.aliases,
            [
                "Alice Smith",
                "alice",
                "alice.smith",
                "alice.smith@example.com",
                "asmith"
            ]
        );
        assert_eq!(alice.attrs[attrs::ROLE], "Engineering Manager");
        assert_eq!(alice.attrs[attrs::EMAIL], "alice.smith@example.com");
        let team = node(unit, "team:payments-platform");
        assert_eq!(
            team.aliases,
            [
                "Payments Platform",
                "acme/payments-platform",
                "payments-platform",
                "payments@example.com"
            ]
        );
        assert_eq!(team.attrs[attrs::EMAIL], "payments@example.com");
        // An explicit organisation prefix in the handle is kept as written.
        let output = parse(
            "o.json",
            r#"{"teams":[{"name":"Core","handle":"@globex/core-team"}]}"#,
        );
        assert_eq!(
            node(&output.units[0], "team:core-team").aliases,
            ["Core", "core-team", "globex/core-team"]
        );
    }

    const FLAT_JSON: &str = r#"{
      "portfolios": [{"name": "Consumer Payments"}],
      "domains": [{"name": "Billing", "portfolio": "consumer payments", "owner": ["bjones", "Ghost Team"]}],
      "components": [{"name": "Ledger", "domain": "Billing"}],
      "services": [
        {"name": "payments-api", "component": "Ledger", "owner": "payments-platform"},
        {"name": "orphan", "component": "Nowhere", "owner": "@ghost"}
      ],
      "teams": [{"name": "Payments Platform", "handle": "payments-platform", "members": ["nobody@example.com"]}],
      "people": [{"name": "Bob Jones", "handle": "bjones", "reports_to": "The Boss", "team": ["Payments Platform"]}]
    }"#;

    const FLAT_TOML: &str = r#"
[[portfolios]]
name = "Consumer Payments"

[[domains]]
name = "Billing"
portfolio = "consumer payments"
owner = ["bjones", "Ghost Team"]

[[components]]
name = "Ledger"
domain = "Billing"

[[services]]
name = "payments-api"
component = "Ledger"
owner = "payments-platform"

[[services]]
name = "orphan"
component = "Nowhere"
owner = "@ghost"

[[teams]]
name = "Payments Platform"
handle = "payments-platform"
members = ["nobody@example.com"]

[[people]]
name = "Bob Jones"
handle = "bjones"
reports_to = "The Boss"
team = ["Payments Platform"]
"#;

    #[test]
    fn flat_form_and_dangling_references_in_json_and_toml() {
        let json = parse("org.json", FLAT_JSON);
        let unit = &json.units[0];
        assert_eq!(
            edges(unit),
            [
                "component:ledger -contains-> service:payments-api",
                "component:nowhere -contains-> service:orphan",
                "domain:billing -contains-> component:ledger",
                "person:bjones -owns-> domain:billing",
                "person:bjones -member_of-> team:payments-platform",
                "person:bjones -reports_to-> person:the-boss",
                "person:ghost -owns-> service:orphan",
                "person:nobody -member_of-> team:payments-platform",
                "portfolio:consumer-payments -contains-> domain:billing",
                "team:ghost-team -owns-> domain:billing",
                "team:payments-platform -owns-> service:payments-api",
            ]
        );
        // Dangling targets are referenced by key but never asserted.
        for key in [
            "component:nowhere",
            "person:ghost",
            "person:the-boss",
            "team:ghost-team",
        ] {
            assert!(unit.nodes.iter().all(|n| n.key.as_str() != key), "{key}");
        }
        let warned: Vec<_> = json.warnings.iter().map(|w| w.location.as_str()).collect();
        assert_eq!(
            warned,
            [
                "people/Bob Jones",
                "teams/Payments Platform",
                "domains/Billing",
                "services/orphan",
                "services/orphan"
            ]
        );

        // TOML states the same model: identical content apart from the unit key.
        let toml = parse("org.toml", FLAT_TOML);
        assert_eq!(toml.units[0].nodes, unit.nodes);
        assert_eq!(toml.units[0].edges, unit.edges);
        assert_eq!(toml.warnings, json.warnings);
    }

    #[test]
    fn output_is_deterministic_and_independent_of_section_order() {
        let first = parse("org.yaml", NESTED_YAML);
        let second = parse("org.yaml", NESTED_YAML);
        assert_eq!(
            first.units[0].content_hash().unwrap(),
            second.units[0].content_hash().unwrap()
        );
        // The same model written as JSON with sections in another order hashes identically.
        let value: serde_json::Map<String, Value> = serde_yaml_ng::from_str(NESTED_YAML).unwrap();
        let reversed: serde_json::Map<String, Value> = value.into_iter().rev().collect();
        let mut json = parse("org.yaml", &serde_json::to_string(&reversed).unwrap());
        assert_eq!(json.units.remove(0), first.units[0]);
    }

    #[test]
    fn malformed_documents_are_errors_and_bad_entries_warnings() {
        let source = SourceId::from("org");
        assert!(matches!(
            parse_org_model(&source, "org.txt", "x"),
            Err(Error::Invalid(_))
        ));
        for (path, text) in [
            ("org.yaml", "teams: [unclosed"),
            ("org.json", "{"),
            ("org.toml", "teams = ["),
            ("org.yaml", "teams: not-a-list"),
        ] {
            assert!(
                matches!(parse_org_model(&source, path, text), Err(Error::Parse(_))),
                "{path}: {text}"
            );
        }
        let output = parse(
            "org.yml",
            "portfolios:\n  - description: nameless\nteams:\n  - {}\npeople:\n  - role: ghost\ndomains:\n",
        );
        assert!(output.units[0].is_empty());
        assert_eq!(output.warnings.len(), 3);
        assert!(parse("empty.yaml", "").units[0].is_empty());
    }
}
