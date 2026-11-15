//! Kubernetes-style OWNERS and OWNERS_ALIASES.
//!
//! An OWNERS file applies to its directory subtree. Owners of ancestor directories also apply
//! unless a file on the way up sets `options.no_parent_owners`. `filters` restrict approvers and
//! reviewers to paths (relative to the OWNERS file's directory) matching a regular expression.

use std::collections::{BTreeMap, HashMap};

use regex::Regex;
use serde::{Deserialize, Deserializer};

use super::{OwnerMatch, OwnerRole, Parties};
use crate::base::{NodeAssertion, NodeKind, keys};
use crate::engine::ingest::{IngestWarning, party::PersonIdentity};

/// YAML keys are often present but empty (`reviewers:`), which serde sees as an explicit null.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Options {
    #[serde(deserialize_with = "null_default")]
    no_parent_owners: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OwnersBlock {
    #[serde(deserialize_with = "null_default")]
    approvers: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    reviewers: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    labels: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OwnersFile {
    #[serde(flatten)]
    top: OwnersBlock,
    #[serde(deserialize_with = "null_default")]
    options: Options,
    #[serde(deserialize_with = "null_default")]
    filters: BTreeMap<String, OwnersBlock>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AliasesFile {
    #[serde(deserialize_with = "null_default")]
    aliases: BTreeMap<String, Option<Vec<String>>>,
}

struct Filter {
    regex: Regex,
    owners: Vec<(usize, OwnerRole)>,
    labels: Vec<String>,
}

struct DirRules {
    no_parent_owners: bool,
    owners: Vec<(usize, OwnerRole)>,
    labels: Vec<String>,
    filters: Vec<Filter>,
}

/// Compiled OWNERS tree: one hash lookup per ancestor directory of the queried path.
pub(super) struct Compiled {
    dirs: HashMap<String, DirRules>,
}

fn person(handle: &str) -> Option<NodeAssertion> {
    PersonIdentity::new(None, Some(handle), None).assertion()
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Parses the tree. Returns the compiled rules and the `(member, team)` pairs of the aliases.
pub(super) fn parse<'a>(
    files: impl IntoIterator<Item = (&'a str, &'a str)>,
    aliases: Option<&str>,
    parties: &mut Parties,
    warnings: &mut Vec<IngestWarning>,
) -> (Compiled, Vec<(usize, usize)>) {
    let mut members = Vec::new();
    // Alias name (lower-case) -> team party.
    let mut teams: HashMap<String, usize> = HashMap::new();
    if let Some(text) = aliases.filter(|text| !text.trim().is_empty()) {
        match serde_yaml_ng::from_str::<AliasesFile>(text) {
            Ok(file) => {
                for (name, people) in file.aliases {
                    let key = keys::team(&name);
                    if key.as_str() == "team:" {
                        continue;
                    }
                    let team = parties.intern(
                        NodeAssertion::new(NodeKind::Team, key, name.clone()).alias(name.clone()),
                    );
                    teams.insert(name.to_ascii_lowercase(), team);
                    for handle in people.into_iter().flatten() {
                        if let Some(node) = person(&handle) {
                            members.push((parties.intern(node), team));
                        }
                    }
                }
            }
            Err(err) => warnings.push(IngestWarning::new(
                "OWNERS_ALIASES",
                format!("ignored malformed aliases file: {err}"),
            )),
        }
    }

    let mut resolve = |names: &[String], role: OwnerRole, out: &mut Vec<(usize, OwnerRole)>| {
        for name in names {
            let name = name.trim().trim_start_matches('@');
            let party = match teams.get(&name.to_ascii_lowercase()) {
                Some(&team) => Some(team),
                None => person(name).map(|node| parties.intern(node)),
            };
            if let Some(party) = party {
                out.push((party, role));
            }
        }
    };
    let mut block = |block: &OwnersBlock| {
        let mut owners = Vec::new();
        resolve(&block.approvers, OwnerRole::Approver, &mut owners);
        resolve(&block.reviewers, OwnerRole::Reviewer, &mut owners);
        owners
    };

    let mut dirs = HashMap::new();
    for (rel_path, text) in files {
        let path = keys::normalize_path(rel_path);
        let file = if text.trim().is_empty() {
            OwnersFile::default()
        } else {
            match serde_yaml_ng::from_str::<OwnersFile>(text) {
                Ok(file) => file,
                Err(err) => {
                    warnings.push(IngestWarning::new(
                        path,
                        format!("ignored malformed OWNERS file: {err}"),
                    ));
                    continue;
                }
            }
        };
        let mut filters = Vec::new();
        for (pattern, filter) in file.filters {
            match Regex::new(&pattern) {
                Ok(regex) => filters.push(Filter {
                    regex,
                    owners: block(&filter),
                    labels: filter.labels,
                }),
                Err(err) => warnings.push(IngestWarning::new(
                    path.clone(),
                    format!("ignored filter with invalid regular expression `{pattern}`: {err}"),
                )),
            }
        }
        dirs.insert(
            parent_dir(&path).to_owned(),
            DirRules {
                no_parent_owners: file.options.no_parent_owners,
                owners: block(&file.top),
                labels: file.top.labels,
                filters,
            },
        );
    }
    (Compiled { dirs }, members)
}

impl Compiled {
    /// Visits the applicable OWNERS directories from the deepest one up to the root.
    fn walk<'s>(&'s self, path: &'s str, mut visit: impl FnMut(&'s str, &'s DirRules, &'s str)) {
        let mut dir = parent_dir(path);
        loop {
            if let Some(rules) = self.dirs.get(dir) {
                let relative = if dir.is_empty() {
                    path
                } else {
                    &path[dir.len() + 1..]
                };
                visit(dir, rules, relative);
                if rules.no_parent_owners {
                    break;
                }
            }
            if dir.is_empty() {
                break;
            }
            dir = parent_dir(dir);
        }
    }

    pub(super) fn matches(&self, path: &str, parties: &Parties, out: &mut Vec<OwnerMatch>) {
        self.walk(path, |dir, rules, relative| {
            let file = if dir.is_empty() {
                "OWNERS".to_owned()
            } else {
                format!("{dir}/OWNERS")
            };
            let mut push = |owners: &[(usize, OwnerRole)], rule: &str| {
                for &(party, role) in owners {
                    out.push(OwnerMatch {
                        party: parties.get(party).clone(),
                        role,
                        component: None,
                        rule: rule.to_owned(),
                    });
                }
            };
            push(&rules.owners, &file);
            for filter in &rules.filters {
                if filter.regex.is_match(relative) {
                    push(
                        &filter.owners,
                        &format!("{file} filter {}", filter.regex.as_str()),
                    );
                }
            }
        });
    }

    pub(super) fn labels(&self, path: &str) -> Vec<String> {
        let mut labels: Vec<String> = Vec::new();
        self.walk(path, |_, rules, relative| {
            let filtered = rules
                .filters
                .iter()
                .filter(|filter| filter.regex.is_match(relative))
                .flat_map(|filter| &filter.labels);
            for label in rules.labels.iter().chain(filtered) {
                if !labels.contains(label) {
                    labels.push(label.clone());
                }
            }
        });
        labels
    }
}

#[cfg(test)]
mod tests {
    use super::super::{OwnerRole, OwnershipRules, parse_owners_tree};

    fn tree() -> OwnershipRules {
        let files = [
            (
                "OWNERS",
                "approvers:\n  - root-approver\n  - sig-leads\nreviewers:\nlabels:\n  - area/root\n",
            ),
            (
                "pkg/api/OWNERS",
                "approvers: [api-owner]\nreviewers: [API-Reviewers]\nlabels: [sig/api]\n",
            ),
            (
                "pkg/api/internal/OWNERS",
                "options:\n  no_parent_owners: true\napprovers: [internal-only]\n",
            ),
            (
                "docs/OWNERS",
                "filters:\n  \"\\\\.md$\":\n    approvers: [doc-writer]\n    labels: [kind/docs]\n  \"^api/\":\n    reviewers: [api-owner]\n  \"(\":\n    approvers: [never]\n",
            ),
            ("broken/OWNERS", "approvers: {not: [a list\n"),
        ];
        let aliases = "aliases:\n  sig-leads:\n    - alice\n    - bob\n  api-reviewers: [carol]\n";
        parse_owners_tree(files, Some(aliases)).unwrap()
    }

    fn owners(rules: &OwnershipRules, path: &str) -> Vec<(String, OwnerRole)> {
        rules
            .owners_of(path)
            .into_iter()
            .map(|m| (m.party.key.to_string(), m.role))
            .collect()
    }

    #[test]
    fn inheritance_no_parent_owners_filters_and_aliases() {
        let rules = tree();
        let approver = |key: &str| (key.to_owned(), OwnerRole::Approver);
        let reviewer = |key: &str| (key.to_owned(), OwnerRole::Reviewer);
        // Deepest directory first, then ancestors; aliases are teams.
        assert_eq!(
            owners(&rules, "pkg/api/v1/types.go"),
            [
                approver("person:api-owner"),
                reviewer("team:api-reviewers"),
                approver("person:root-approver"),
                approver("team:sig-leads"),
            ]
        );
        // no_parent_owners cuts the chain.
        assert_eq!(
            owners(&rules, "pkg/api/internal/x.go"),
            [approver("person:internal-only")]
        );
        // Filters match the path relative to the OWNERS directory.
        assert_eq!(
            owners(&rules, "docs/api/guide.md"),
            [
                approver("person:doc-writer"),
                reviewer("person:api-owner"),
                approver("person:root-approver"),
                approver("team:sig-leads"),
            ]
        );
        assert_eq!(owners(&rules, "docs/img/a.png").len(), 2);
        assert_eq!(
            rules.labels_of("docs/x.md"),
            ["kind/docs".to_owned(), "area/root".to_owned()]
        );
        // The malformed file and the invalid filter regex are reported.
        assert_eq!(rules.warnings().len(), 2);
        assert_eq!(owners(&rules, "broken/x").len(), 2);
    }

    #[test]
    fn aliases_become_teams_with_members() {
        let unit = tree().definitions_unit(&"k8s".into(), "OWNERS");
        let team = unit
            .nodes
            .iter()
            .find(|n| n.key.as_str() == "team:sig-leads")
            .unwrap();
        assert_eq!(team.name, "sig-leads");
        let members: Vec<_> = unit
            .edges
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .filter(|e| e["kind"] == "member_of")
            .map(|e| {
                format!(
                    "{}>{}",
                    e["from"]["key"].as_str().unwrap(),
                    e["to"]["key"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(
            members,
            [
                "person:alice>team:sig-leads",
                "person:bob>team:sig-leads",
                "person:carol>team:api-reviewers"
            ]
        );
    }
}
