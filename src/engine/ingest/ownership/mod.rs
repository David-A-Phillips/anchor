//! Ownership files: CODEOWNERS, Linux-style MAINTAINERS and Kubernetes-style OWNERS.
//!
//! Each format is parsed into an [`OwnershipRules`] value holding pre-compiled matchers, so that
//! asking "who owns this path?" costs microseconds even for thousands of rules:
//!
//! | Format | Constructor | Semantics |
//! |--------|-------------|-----------|
//! | CODEOWNERS (GitHub, GitLab) | [`parse_codeowners`] | last matching pattern wins (per GitLab section) |
//! | MAINTAINERS (Linux) | [`parse_maintainers`] | every matching entry applies |
//! | OWNERS + OWNERS_ALIASES (Kubernetes) | [`parse_owners_tree`] | directory subtree, inherited from ancestors |
//!
//! The rules feed the graph in two ways. [`OwnershipRules::definitions_unit`] is the unit of the
//! ownership file itself: the teams and people it names and, for MAINTAINERS, one Component per
//! entry. [`OwnershipRules::file_edges`] and [`OwnershipRules::file_parties`] give the ingestion
//! pipeline what to add to each *file's* unit, so ownership of a file is replaced together with
//! the file.
//!
//! Parsing never fails on malformed lines: they are skipped and reported through
//! [`OwnershipRules::warnings`].

mod codeowners;
mod maintainers;
mod owners;

use std::collections::HashMap;

use serde_json::Value;

use crate::base::{
    ChunkKind, EdgeAssertion, EdgeKind, NodeAssertion, NodeKey, NodeKind, Result, SourceId, Unit,
    UnitKind, attrs, keys,
};
use crate::engine::ingest::IngestWarning;

/// Edge attribute naming the rule of the ownership file that produced an edge.
pub const ATTR_RULE: &str = "rule";
/// Component attribute: the `F:` patterns of a MAINTAINERS entry.
pub const ATTR_PATTERNS: &str = "patterns";
/// Component attribute: the `X:` patterns of a MAINTAINERS entry.
pub const ATTR_EXCLUDED_PATTERNS: &str = "excluded_patterns";
/// Component attribute: the `N:` regular expressions of a MAINTAINERS entry.
pub const ATTR_NAME_PATTERNS: &str = "name_patterns";
/// Component attribute: the `K:` content keywords of a MAINTAINERS entry (recorded, not evaluated).
pub const ATTR_KEYWORDS: &str = "keywords";

/// How a party relates to the files a rule covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OwnerRole {
    /// CODEOWNERS owner.
    Owner,
    /// MAINTAINERS `M:`.
    Maintainer,
    /// MAINTAINERS `R:` or OWNERS `reviewers`.
    Reviewer,
    /// OWNERS `approvers`.
    Approver,
    /// MAINTAINERS `L:` mailing list.
    List,
}

impl OwnerRole {
    /// Stable lower-case name, used as the `role` edge attribute.
    pub const fn as_str(self) -> &'static str {
        match self {
            OwnerRole::Owner => "owner",
            OwnerRole::Maintainer => "maintainer",
            OwnerRole::Reviewer => "reviewer",
            OwnerRole::Approver => "approver",
            OwnerRole::List => "list",
        }
    }

    /// Weight of the `Owns` edge: `1.0` for owners, maintainers and approvers, `0.8` for lists,
    /// `0.6` for reviewers.
    pub const fn weight(self) -> f32 {
        match self {
            OwnerRole::Owner | OwnerRole::Maintainer | OwnerRole::Approver => 1.0,
            OwnerRole::List => 0.8,
            OwnerRole::Reviewer => 0.6,
        }
    }
}

/// One party responsible for a path, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnerMatch {
    /// The Team or Person, ready to be asserted by a unit.
    pub party: NodeAssertion,
    /// Their role.
    pub role: OwnerRole,
    /// The MAINTAINERS entry's Component, when the match comes from one.
    pub component: Option<NodeKey>,
    /// Human-readable description of the matching rule, for provenance.
    pub rule: String,
}

/// The ownership file formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OwnershipFileKind {
    /// GitHub/GitLab CODEOWNERS.
    Codeowners,
    /// Linux-style MAINTAINERS.
    Maintainers,
    /// Kubernetes-style OWNERS.
    Owners,
    /// Kubernetes-style OWNERS_ALIASES.
    OwnersAliases,
}

/// Which ownership file, if any, a repository-relative path is.
///
/// CODEOWNERS is honoured in the root, `.github/`, `.gitlab/` and `docs/`; MAINTAINERS and
/// OWNERS_ALIASES only in the root (their patterns are root-relative); OWNERS in any directory.
pub fn ownership_file_kind(rel_path: &str) -> Option<OwnershipFileKind> {
    let path = keys::normalize_path(rel_path);
    match path.as_str() {
        "CODEOWNERS" | ".github/CODEOWNERS" | ".gitlab/CODEOWNERS" | "docs/CODEOWNERS" => {
            Some(OwnershipFileKind::Codeowners)
        }
        "MAINTAINERS" => Some(OwnershipFileKind::Maintainers),
        "OWNERS_ALIASES" => Some(OwnershipFileKind::OwnersAliases),
        other if other == "OWNERS" || other.ends_with("/OWNERS") => Some(OwnershipFileKind::Owners),
        _ => None,
    }
}

/// The parties named by an ownership file, interned so that rules refer to them by index.
#[derive(Default)]
pub(crate) struct Parties {
    list: Vec<NodeAssertion>,
    index: HashMap<NodeKey, usize>,
}

impl Parties {
    /// Adds a party, merging aliases and attributes into an earlier mention with the same key.
    fn intern(&mut self, node: NodeAssertion) -> usize {
        if let Some(&existing) = self.index.get(&node.key) {
            let entry = &mut self.list[existing];
            for alias in node.aliases {
                if !entry.aliases.contains(&alias) {
                    entry.aliases.push(alias);
                }
            }
            for (name, value) in node.attrs {
                entry.attrs.entry(name).or_insert(value);
            }
            existing
        } else {
            self.index.insert(node.key.clone(), self.list.len());
            self.list.push(node);
            self.list.len() - 1
        }
    }

    fn get(&self, index: usize) -> &NodeAssertion {
        &self.list[index]
    }
}

enum Matcher {
    Codeowners(codeowners::Compiled),
    Maintainers(maintainers::Compiled),
    Owners(owners::Compiled),
}

/// A compiled ownership rule set. Cheap to query, immutable, `Send + Sync`.
pub struct OwnershipRules {
    parties: Parties,
    matcher: Matcher,
    /// `(member, team)` party indices from OWNERS_ALIASES.
    members: Vec<(usize, usize)>,
    warnings: Vec<IngestWarning>,
}

impl std::fmt::Debug for OwnershipRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnershipRules")
            .field("kind", &self.kind())
            .field("parties", &self.parties.list.len())
            .field("warnings", &self.warnings.len())
            .finish()
    }
}

/// Compiles a CODEOWNERS file (GitHub or GitLab syntax).
///
/// Supported: comments (whole-line and trailing), `\ ` escapes, gitignore-style patterns, GitLab
/// `[Section]`, `^[Optional]` and `[Section][N]` headers with default owners. GitLab `!` exclusion
/// patterns are not supported. Fails only if the pattern set as a whole cannot be compiled.
pub fn parse_codeowners(text: &str) -> Result<OwnershipRules> {
    let mut parties = Parties::default();
    let mut warnings = Vec::new();
    let compiled = codeowners::parse(text, &mut parties, &mut warnings)?;
    Ok(OwnershipRules {
        parties,
        matcher: Matcher::Codeowners(compiled),
        members: Vec::new(),
        warnings,
    })
}

/// Compiles a Linux-style MAINTAINERS file.
///
/// `N:` expressions use the `regex` crate's syntax; Perl-only constructs (look-around,
/// back-references) are skipped with a warning. `K:` keywords are recorded but never evaluated.
pub fn parse_maintainers(text: &str) -> Result<OwnershipRules> {
    let mut parties = Parties::default();
    let mut warnings = Vec::new();
    let compiled = maintainers::parse(text, &mut parties, &mut warnings)?;
    Ok(OwnershipRules {
        parties,
        matcher: Matcher::Maintainers(compiled),
        members: Vec::new(),
        warnings,
    })
}

/// Compiles a tree of Kubernetes-style OWNERS files.
///
/// `files` yields `(repository-relative path of an OWNERS file, its text)`; `aliases` is the text
/// of the root OWNERS_ALIASES file, if any. Malformed files are skipped with a warning. The
/// `Result` is always `Ok` today and exists for symmetry with the other constructors.
pub fn parse_owners_tree<'a>(
    files: impl IntoIterator<Item = (&'a str, &'a str)>,
    aliases: Option<&str>,
) -> Result<OwnershipRules> {
    let mut parties = Parties::default();
    let mut warnings = Vec::new();
    let (compiled, members) = owners::parse(files, aliases, &mut parties, &mut warnings);
    Ok(OwnershipRules {
        parties,
        matcher: Matcher::Owners(compiled),
        members,
        warnings,
    })
}

fn string_array(values: &[String]) -> Value {
    Value::Array(values.iter().cloned().map(Value::String).collect())
}

impl OwnershipRules {
    /// The format these rules were parsed from ([`OwnershipFileKind::Owners`] for an OWNERS tree).
    pub fn kind(&self) -> OwnershipFileKind {
        match self.matcher {
            Matcher::Codeowners(_) => OwnershipFileKind::Codeowners,
            Matcher::Maintainers(_) => OwnershipFileKind::Maintainers,
            Matcher::Owners(_) => OwnershipFileKind::Owners,
        }
    }

    /// Problems found while parsing (skipped lines, invalid patterns, malformed files). Locations
    /// are `line N` for single-file formats and the OWNERS file path for trees.
    pub fn warnings(&self) -> &[IngestWarning] {
        &self.warnings
    }

    /// Every Team and Person the ownership file names.
    pub fn parties(&self) -> &[NodeAssertion] {
        &self.parties.list
    }

    /// The parties responsible for a repository-relative path, in rule order (CODEOWNERS:
    /// sections in file order; MAINTAINERS: entries in file order; OWNERS: nearest directory
    /// first). A party appears once per distinct role and component. Empty when nobody owns it.
    pub fn owners_of(&self, rel_path: &str) -> Vec<OwnerMatch> {
        let path = keys::normalize_path(rel_path);
        let mut out = Vec::new();
        match &self.matcher {
            Matcher::Codeowners(compiled) => compiled.matches(&path, &self.parties, &mut out),
            Matcher::Maintainers(compiled) => compiled.matches(&path, &self.parties, &mut out),
            Matcher::Owners(compiled) => compiled.matches(&path, &self.parties, &mut out),
        }
        // Matches are few, so a quadratic scan is cheaper than hashing.
        let mut unique: Vec<OwnerMatch> = Vec::with_capacity(out.len());
        for candidate in out {
            let duplicate = unique.iter().any(|kept| {
                kept.party.key == candidate.party.key
                    && kept.role == candidate.role
                    && kept.component == candidate.component
            });
            if !duplicate {
                unique.push(candidate);
            }
        }
        unique
    }

    /// The labels OWNERS files attach to a path (nearest directory first, without duplicates).
    /// Always empty for CODEOWNERS and MAINTAINERS.
    pub fn labels_of(&self, rel_path: &str) -> Vec<String> {
        match &self.matcher {
            Matcher::Owners(compiled) => compiled.labels(&keys::normalize_path(rel_path)),
            _ => Vec::new(),
        }
    }

    /// The unit of the ownership file itself, in canonical form (key
    /// `keys::unit(source, UnitKind::Ownership, rel_path)`).
    ///
    /// It asserts every Team and Person the file names, `MemberOf` edges for OWNERS aliases and,
    /// for MAINTAINERS, one Component per entry with its owners and a Description chunk. For an
    /// OWNERS tree pass one stable path for the whole tree (the root `OWNERS` or `OWNERS_ALIASES`).
    pub fn definitions_unit(&self, source: &SourceId, rel_path: &str) -> Unit {
        let path = keys::normalize_path(rel_path);
        let mut unit = Unit::new(
            keys::unit(source, UnitKind::Ownership, &path),
            source.clone(),
            UnitKind::Ownership,
        );
        for party in &self.parties.list {
            unit.add_node(party.clone());
        }
        for &(member, team) in &self.members {
            unit.add_edge(EdgeAssertion::new(
                &self.parties.get(member).key,
                EdgeKind::MemberOf,
                &self.parties.get(team).key,
            ));
        }
        if let Matcher::Maintainers(compiled) = &self.matcher {
            for entry in &compiled.entries {
                self.add_component(&mut unit, entry);
            }
        }
        unit.finish()
    }

    fn add_component(&self, unit: &mut Unit, entry: &maintainers::Entry) {
        let mut node = NodeAssertion::new(
            NodeKind::Component,
            entry.component.clone(),
            entry.title.clone(),
        )
        .alias(entry.title.clone())
        .attr_opt(attrs::STATUS, entry.status.clone());
        let lists = [
            (ATTR_PATTERNS, &entry.files),
            (ATTR_EXCLUDED_PATTERNS, &entry.excluded),
            (ATTR_NAME_PATTERNS, &entry.name_patterns),
            (ATTR_KEYWORDS, &entry.keywords),
        ];
        for (name, values) in lists {
            if !values.is_empty() {
                node = node.attr(name, string_array(values));
            }
        }
        for (name, values) in &entry.extra {
            // The web page is a single well-known attribute; the others keep every value.
            node = if name == attrs::URL {
                node.attr_opt(attrs::URL, values.first().cloned())
            } else {
                node.attr(name.clone(), string_array(values))
            };
        }
        let component = unit.add_node(node);

        let mut summary = entry.title.clone();
        if let Some(status) = &entry.status {
            summary.push_str(&format!("\nStatus: {status}"));
        }
        for (role, label) in [
            (OwnerRole::Maintainer, "Maintainers"),
            (OwnerRole::Reviewer, "Reviewers"),
            (OwnerRole::List, "Mailing lists"),
        ] {
            let names: Vec<&str> = entry
                .people
                .iter()
                .filter(|(_, r)| *r == role)
                .map(|&(party, _)| self.parties.get(party).name.as_str())
                .collect();
            if !names.is_empty() {
                summary.push_str(&format!("\n{label}: {}", names.join(", ")));
            }
        }
        for (label, values) in [
            ("Files", &entry.files),
            ("Excluded", &entry.excluded),
            ("File name patterns", &entry.name_patterns),
            ("Keywords", &entry.keywords),
        ] {
            if !values.is_empty() {
                summary.push_str(&format!("\n{label}: {}", values.join(", ")));
            }
        }
        if let Some(url) = entry.extra.get(attrs::URL).and_then(|urls| urls.first()) {
            summary.push_str(&format!("\nWeb: {url}"));
        }
        unit.add_chunk(&component, ChunkKind::Description, summary);

        // Strongest role first: canonicalisation keeps the heaviest of duplicate edges, and this
        // way its role attribute is the strongest one too.
        let mut people = entry.people.clone();
        people.sort_by(|a, b| b.1.weight().total_cmp(&a.1.weight()));
        for (party, role) in people {
            unit.add_edge(
                EdgeAssertion::new(&self.parties.get(party).key, EdgeKind::Owns, &component)
                    .weight(role.weight())
                    .attr(attrs::ROLE, role.as_str()),
            );
        }
    }

    /// The edges to add to the unit of the file `file_key` at `rel_path`:
    ///
    /// * `party --Owns--> file`, one per party (its strongest role), with the role's
    ///   [weight](OwnerRole::weight) and attributes `role` and [`rule`](ATTR_RULE);
    /// * `component --Contains--> file` for each matching MAINTAINERS entry.
    ///
    /// The parties must be asserted somewhere for the edges to materialise: add
    /// [`file_parties`](Self::file_parties) to the same unit (the definitions unit asserts them
    /// too, but a file unit should not depend on it).
    pub fn file_edges(&self, file_key: &NodeKey, rel_path: &str) -> Vec<EdgeAssertion> {
        let matches = self.owners_of(rel_path);
        let mut best: Vec<&OwnerMatch> = Vec::new();
        for candidate in &matches {
            match best
                .iter_mut()
                .find(|kept| kept.party.key == candidate.party.key)
            {
                Some(kept) if candidate.role.weight() > kept.role.weight() => *kept = candidate,
                Some(_) => {}
                None => best.push(candidate),
            }
        }
        let mut edges: Vec<EdgeAssertion> = best
            .into_iter()
            .map(|owner| {
                EdgeAssertion::new(&owner.party.key, EdgeKind::Owns, file_key)
                    .weight(owner.role.weight())
                    .attr(attrs::ROLE, owner.role.as_str())
                    .attr(ATTR_RULE, owner.rule.clone())
            })
            .collect();
        let mut components: Vec<&NodeKey> = Vec::new();
        for component in matches.iter().filter_map(|m| m.component.as_ref()) {
            if !components.contains(&component) {
                components.push(component);
                edges.push(EdgeAssertion::new(component, EdgeKind::Contains, file_key));
            }
        }
        edges
    }

    /// The Team and Person assertions referenced by [`file_edges`](Self::file_edges) for the same
    /// path, without duplicates, for inclusion in the file's unit.
    pub fn file_parties(&self, rel_path: &str) -> Vec<NodeAssertion> {
        let mut parties: Vec<NodeAssertion> = Vec::new();
        for owner in self.owners_of(rel_path) {
            if !parties.iter().any(|kept| kept.key == owner.party.key) {
                parties.push(owner.party);
            }
        }
        parties
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn recognises_ownership_files() {
        use OwnershipFileKind::*;
        for (path, kind) in [
            ("CODEOWNERS", Some(Codeowners)),
            ("./.github/CODEOWNERS", Some(Codeowners)),
            ("docs/CODEOWNERS", Some(Codeowners)),
            ("src/CODEOWNERS", None),
            ("MAINTAINERS", Some(Maintainers)),
            ("OWNERS", Some(Owners)),
            ("pkg/api/OWNERS", Some(Owners)),
            ("OWNERS_ALIASES", Some(OwnersAliases)),
            ("pkg/NOT_OWNERS", None),
            ("src/main.rs", None),
        ] {
            assert_eq!(ownership_file_kind(path), kind, "{path}");
        }
    }

    #[test]
    fn codeowners_definitions_unit_and_file_edges() {
        let rules = parse_codeowners("* @acme/platform alice@example.com\n/docs/ @bob\n").unwrap();
        let source = SourceId::from("repo");
        let unit = rules.definitions_unit(&source, ".github/CODEOWNERS");
        assert_eq!(unit.kind, UnitKind::Ownership);
        assert_eq!(
            unit.key,
            keys::unit(&source, UnitKind::Ownership, ".github/CODEOWNERS")
        );
        let keys_: Vec<_> = unit.nodes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(keys_, ["person:alice", "person:bob", "team:platform"]);
        assert!(unit.nodes[2].aliases.contains(&"acme/platform".to_owned()));
        assert!(unit.edges.is_empty() && unit.chunks.is_empty());
        // Same input, same content hash.
        let again = parse_codeowners("* @acme/platform alice@example.com\n/docs/ @bob\n")
            .unwrap()
            .definitions_unit(&source, ".github/CODEOWNERS");
        assert_eq!(unit.content_hash().unwrap(), again.content_hash().unwrap());

        let file = keys::file(&source, "src/lib.rs");
        let edges = rules.file_edges(&file, "src/lib.rs");
        assert_eq!(edges.len(), 2);
        for edge in &edges {
            assert_eq!(edge.kind, EdgeKind::Owns);
            assert_eq!(edge.to, (&file).into());
            assert_eq!(edge.weight, 1.0);
            assert_eq!(edge.attrs[attrs::ROLE], "owner");
            assert!(edge.attrs[ATTR_RULE].as_str().unwrap().contains("line 1"));
        }
        let parties = rules.file_parties("src/lib.rs");
        assert_eq!(parties.len(), 2);
        assert_eq!(parties[0].kind, NodeKind::Team);
        assert_eq!(
            rules.file_parties("docs/a.md")[0].key.as_str(),
            "person:bob"
        );
    }

    #[test]
    fn maintainers_definitions_unit_and_file_edges() {
        let text = "\
ACME LEDGER
M:\tAlice Smith <alice@example.com>
M:\tRita Both <rita@example.com>
R:\tRita Both <rita@example.com>
R:\tBob Jones <bob@example.com>
L:\tledger@lists.example.com
S:\tSupported
W:\thttps://ledger.example.com
F:\tledger/
";
        let rules = parse_maintainers(text).unwrap();
        let source = SourceId::from("repo");
        let unit = rules.definitions_unit(&source, "MAINTAINERS");
        let component = keys::component("ACME LEDGER");
        let list = unit
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Team)
            .unwrap();
        assert_eq!(list.aliases, ["ledger@lists.example.com"]);
        assert_eq!(list.attrs[attrs::EMAIL], "ledger@lists.example.com");
        let alice = unit.nodes.iter().find(|n| n.name == "Alice Smith").unwrap();
        assert!(alice.aliases.contains(&"alice@example.com".to_owned()));

        let owns: Vec<_> = unit
            .edges
            .iter()
            .map(|e| {
                assert_eq!((e.kind, &e.to), (EdgeKind::Owns, &(&component).into()));
                let from = serde_json::to_value(&e.from).unwrap()["key"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                (
                    from,
                    e.weight,
                    e.attrs[attrs::ROLE].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            owns,
            [
                ("person:alice".to_owned(), 1.0, "maintainer".to_owned()),
                ("person:bob".to_owned(), 0.6, "reviewer".to_owned()),
                // Maintainer and reviewer at once: the stronger role is kept.
                ("person:rita".to_owned(), 1.0, "maintainer".to_owned()),
                (
                    "team:ledger-lists-example-com".to_owned(),
                    0.8,
                    "list".to_owned()
                ),
            ]
        );
        assert_eq!(unit.chunks.len(), 1);
        assert_eq!(unit.chunks[0].kind, ChunkKind::Description);
        let summary = &unit.chunks[0].text;
        for part in [
            "ACME LEDGER",
            "Status: Supported",
            "Alice Smith",
            "Files: ledger/",
        ] {
            assert!(summary.contains(part), "{summary}");
        }

        let file = keys::file(&source, "ledger/src/post.rs");
        let edges = rules.file_edges(&file, "ledger/src/post.rs");
        let shape: Vec<_> = edges.iter().map(|e| (e.kind, e.weight)).collect();
        assert_eq!(
            shape,
            [
                (EdgeKind::Owns, 1.0),
                (EdgeKind::Owns, 1.0),
                (EdgeKind::Owns, 0.6),
                (EdgeKind::Owns, 0.8),
                (EdgeKind::Contains, 1.0),
            ]
        );
        assert_eq!(edges[4].from, (&component).into());
        assert_eq!(rules.file_parties("ledger/src/post.rs").len(), 4);
        assert!(rules.file_edges(&file, "other/x.rs").is_empty());
    }

    #[test]
    fn owners_file_edges_use_role_weights() {
        let rules = parse_owners_tree(
            [("svc/OWNERS", "approvers: [amy]\nreviewers: [amy, rob]\n")],
            None,
        )
        .unwrap();
        let file = keys::file(&SourceId::from("repo"), "svc/main.go");
        let edges = rules.file_edges(&file, "svc/main.go");
        let roles: Vec<_> = edges
            .iter()
            .map(|e| (e.attrs[attrs::ROLE].as_str().unwrap(), e.weight))
            .collect();
        assert_eq!(roles, [("approver", 1.0), ("reviewer", 0.6)]);
        assert!(rules.file_edges(&file, "elsewhere/main.go").is_empty());
    }

    /// Matching must stay in the microsecond range with rule sets the size of the Linux
    /// MAINTAINERS file. The bound is deliberately loose (shared CI hosts); typical figures are
    /// far below it.
    #[test]
    fn owners_of_is_fast_with_thousands_of_rules() {
        const ENTRIES: usize = 3000;
        let mut maintainers = String::from("Preamble\n\n");
        let mut codeowners = String::from("* @everyone\n");
        for i in 0..ENTRIES {
            let (a, b) = (i % 40, i);
            maintainers.push_str(&format!(
                "SUBSYSTEM {i}\nM:\tPerson {i} <p{i}@example.com>\nL:\tlist{a}@example.com\nS:\tMaintained\nF:\tdrivers/d{a}/s{b}/\nF:\tinclude/linux/s{b}*.h\nX:\tdrivers/d{a}/s{b}/staging/\n"
            ));
            if i % 15 == 0 {
                maintainers.push_str(&format!("N:\t(^|/)chip{b}[^/]*\\.c$\n"));
            }
            maintainers.push('\n');
            codeowners.push_str(&format!("/drivers/d{a}/s{b}/ @acme/team{a} @p{i}\n"));
            if i % 10 == 0 {
                codeowners.push_str(&format!("*.ext{i} @ext{i}\n"));
            }
        }
        let maintainers = parse_maintainers(&maintainers).unwrap();
        let codeowners = parse_codeowners(&codeowners).unwrap();
        assert!(maintainers.warnings().is_empty() && codeowners.warnings().is_empty());

        let paths: Vec<String> = (0..2000)
            .map(|i| {
                let b = (i * 7) % ENTRIES;
                match i % 4 {
                    0 => format!("drivers/d{}/s{b}/core/main.c", b % 40),
                    1 => format!("include/linux/s{b}_regs.h"),
                    2 => format!("arch/x86/chip{}-foo.c", b - b % 15),
                    _ => format!("fs/unowned/file{i}.c"),
                }
            })
            .collect();
        for (name, rules) in [("MAINTAINERS", &maintainers), ("CODEOWNERS", &codeowners)] {
            let started = Instant::now();
            let found: usize = paths.iter().map(|path| rules.owners_of(path).len()).sum();
            let per_path = started.elapsed() / paths.len() as u32;
            assert!(found >= 1000, "{name}: {found} matches");
            assert!(
                per_path.as_micros() < 250,
                "{name}: owners_of took {per_path:?} per path"
            );
        }
        assert_eq!(
            maintainers.owners_of("drivers/d7/s7/staging/x.c"),
            Vec::new()
        );
        assert_eq!(codeowners.owners_of("drivers/d7/s7/x.c").len(), 2);
    }
}
