//! Linux-style MAINTAINERS.
//!
//! The file is a preamble followed by entries: a title line and typed lines (`M:`, `R:`, `L:`,
//! `S:`, `W:`, `T:`, `F:`, `X:`, `N:`, `K:`, ...). Unlike CODEOWNERS, **every** entry whose
//! patterns match a file applies to it.

use std::collections::BTreeMap;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::{Regex, RegexSet};

use super::{OwnerMatch, OwnerRole, Parties};
use crate::base::{Error, NodeAssertion, NodeKey, NodeKind, Result, attrs, keys};
use crate::engine::ingest::{IngestWarning, party::PersonIdentity};

/// One MAINTAINERS entry.
pub(super) struct Entry {
    pub(super) title: String,
    pub(super) component: NodeKey,
    pub(super) people: Vec<(usize, OwnerRole)>,
    pub(super) status: Option<String>,
    pub(super) files: Vec<String>,
    pub(super) excluded: Vec<String>,
    pub(super) name_patterns: Vec<String>,
    pub(super) keywords: Vec<String>,
    /// Remaining typed lines by attribute name (`url`, `tree`, `bugs`, ...).
    pub(super) extra: BTreeMap<String, Vec<String>>,
}

/// Compiled MAINTAINERS entries.
pub(super) struct Compiled {
    pub(super) entries: Vec<Entry>,
    files: GlobSet,
    /// `(entry, index into entry.files)` of each glob in `files`.
    file_origin: Vec<(u32, u32)>,
    excluded: GlobSet,
    excluded_entry: Vec<u32>,
    names: RegexSet,
    /// `(entry, index into entry.name_patterns)` of each regex in `names`.
    name_origin: Vec<(u32, u32)>,
}

/// A typed line: an upper-case letter at column 0, a colon, then whitespace or end of line.
fn typed_line(line: &str) -> Option<(char, &str)> {
    let bytes = line.as_bytes();
    let typed = bytes.len() >= 2
        && bytes[0].is_ascii_uppercase()
        && bytes[1] == b':'
        && bytes.get(2).is_none_or(u8::is_ascii_whitespace);
    typed.then(|| (bytes[0] as char, line[2..].trim()))
}

/// Attribute name for typed lines that have no dedicated handling.
fn extra_name(letter: char) -> String {
    match letter {
        'W' => attrs::URL.to_owned(),
        'T' => "tree".to_owned(),
        'B' => "bugs".to_owned(),
        'C' => "chat".to_owned(),
        'P' => "profile".to_owned(),
        'Q' => "patchwork".to_owned(),
        other => format!("maintainers_{}", other.to_ascii_lowercase()),
    }
}

/// Only `*` and `?` are wildcards in MAINTAINERS; everything else is literal.
fn escape_glob(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        match c {
            '[' | ']' | '{' | '}' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            c => out.push(c),
        }
    }
    out
}

/// Globs for an `F:`/`X:` pattern. A trailing slash covers the whole tree. Without one the
/// pattern names a file, but entries commonly list directories without the slash and we cannot
/// stat the tree here, so a wildcard-free last segment also covers everything below it.
fn globs_for(pattern: &str) -> Vec<String> {
    let pattern = pattern.trim_start_matches('/');
    let escaped = escape_glob(pattern);
    if pattern.ends_with('/') {
        return vec![format!("{escaped}**")];
    }
    let last = pattern.rsplit('/').next().unwrap_or(pattern);
    if last.contains(['*', '?']) {
        vec![escaped]
    } else {
        vec![format!("{escaped}/**"), escaped]
    }
}

fn add_globs(
    builder: &mut GlobSetBuilder,
    pattern: &str,
    line: usize,
    warnings: &mut Vec<IngestWarning>,
) -> usize {
    let mut added = 0;
    for glob in globs_for(pattern) {
        match GlobBuilder::new(&glob)
            .literal_separator(true)
            .backslash_escape(false)
            .build()
        {
            Ok(glob) => {
                builder.add(glob);
                added += 1;
            }
            Err(err) => warnings.push(IngestWarning::new(
                format!("line {line}"),
                format!("ignored invalid pattern `{pattern}`: {err}"),
            )),
        }
    }
    added
}

/// The Team assertion of an `L:` line: the address is the identity (two lists may share a local
/// part on different hosts), and trailing remarks such as `(moderated)` are dropped.
fn list_assertion(value: &str) -> Option<NodeAssertion> {
    let address = value
        .split_once('(')
        .map_or(value, |(before, _)| before)
        .trim()
        .trim_matches(['<', '>'])
        .to_ascii_lowercase();
    let key = keys::team(&address);
    if key.as_str() == "team:" {
        return None;
    }
    let mut node = NodeAssertion::new(NodeKind::Team, key, address.clone()).alias(address.clone());
    if address.contains('@') && !address.contains("://") {
        node = node.attr(attrs::EMAIL, address);
    } else {
        node = node.attr(attrs::URL, address);
    }
    Some(node)
}

pub(super) fn parse(
    text: &str,
    parties: &mut Parties,
    warnings: &mut Vec<IngestWarning>,
) -> Result<Compiled> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut files = GlobSetBuilder::new();
    let mut file_origin = Vec::new();
    let mut excluded = GlobSetBuilder::new();
    let mut excluded_entry = Vec::new();
    let mut names = Vec::new();
    let mut name_origin = Vec::new();

    // A title is the unindented, untyped line directly above a typed line. Everything before the
    // first such pair is preamble (the kernel's preamble documents the line types, indented).
    let mut title: Option<&str> = None;
    let mut open = false;
    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.trim_end();
        let Some((letter, value)) = typed_line(line) else {
            let candidate = !line.is_empty() && !line.starts_with(char::is_whitespace);
            title = candidate.then_some(line);
            continue;
        };
        if let Some(title) = title.take() {
            let component = keys::component(title);
            open = component.as_str() != "component:";
            if open {
                entries.push(Entry {
                    title: title.trim().to_owned(),
                    component,
                    people: Vec::new(),
                    status: None,
                    files: Vec::new(),
                    excluded: Vec::new(),
                    name_patterns: Vec::new(),
                    keywords: Vec::new(),
                    extra: BTreeMap::new(),
                });
            } else {
                warnings.push(IngestWarning::new(
                    format!("line {line_no}"),
                    format!("ignored entry with unusable title `{title}`"),
                ));
            }
        }
        if !open || value.is_empty() {
            continue;
        }
        let entry_index = u32::try_from(entries.len() - 1)
            .map_err(|_| Error::invalid("MAINTAINERS file has too many entries"))?;
        let Some(entry) = entries.last_mut() else {
            continue;
        };
        match letter {
            'M' | 'R' => {
                let role = if letter == 'M' {
                    OwnerRole::Maintainer
                } else {
                    OwnerRole::Reviewer
                };
                match PersonIdentity::parse(value).assertion() {
                    Some(node) => entry.people.push((parties.intern(node), role)),
                    None => warnings.push(IngestWarning::new(
                        format!("line {line_no}"),
                        format!("ignored unparsable person `{value}`"),
                    )),
                }
            }
            'L' => {
                if let Some(node) = list_assertion(value) {
                    entry.people.push((parties.intern(node), OwnerRole::List));
                }
            }
            'S' => {
                entry.status.get_or_insert_with(|| value.to_owned());
            }
            'F' => {
                let pattern = entry.files.len() as u32;
                for _ in 0..add_globs(&mut files, value, line_no, warnings) {
                    file_origin.push((entry_index, pattern));
                }
                entry.files.push(value.to_owned());
            }
            'X' => {
                for _ in 0..add_globs(&mut excluded, value, line_no, warnings) {
                    excluded_entry.push(entry_index);
                }
                entry.excluded.push(value.to_owned());
            }
            'N' => {
                // Validate individually: one bad expression must not poison the whole set.
                match Regex::new(value) {
                    Ok(_) => {
                        names.push(value.to_owned());
                        name_origin.push((entry_index, entry.name_patterns.len() as u32));
                    }
                    Err(err) => warnings.push(IngestWarning::new(
                        format!("line {line_no}"),
                        format!("ignored unsupported regular expression `{value}`: {err}"),
                    )),
                }
                entry.name_patterns.push(value.to_owned());
            }
            'K' => entry.keywords.push(value.to_owned()),
            other => entry
                .extra
                .entry(extra_name(other))
                .or_default()
                .push(value.to_owned()),
        }
    }

    let build = |builder: GlobSetBuilder| {
        builder
            .build()
            .map_err(|err| Error::parse(format!("MAINTAINERS patterns: {err}")))
    };
    Ok(Compiled {
        entries,
        files: build(files)?,
        file_origin,
        excluded: build(excluded)?,
        excluded_entry,
        names: RegexSet::new(&names)
            .map_err(|err| Error::parse(format!("MAINTAINERS name patterns: {err}")))?,
        name_origin,
    })
}

impl Compiled {
    pub(super) fn matches(&self, path: &str, parties: &Parties, out: &mut Vec<OwnerMatch>) {
        // (entry, is N pattern, pattern index); sorted so each entry keeps its first F pattern.
        let mut hits: Vec<(u32, bool, u32)> = self
            .files
            .matches(path)
            .into_iter()
            .map(|glob| {
                let (entry, pattern) = self.file_origin[glob];
                (entry, false, pattern)
            })
            .collect();
        if !self.name_origin.is_empty() {
            hits.extend(self.names.matches(path).into_iter().map(|regex| {
                let (entry, pattern) = self.name_origin[regex];
                (entry, true, pattern)
            }));
        }
        if hits.is_empty() {
            return;
        }
        hits.sort_unstable();
        hits.dedup_by_key(|hit| hit.0);
        let excluded: Vec<u32> = self
            .excluded
            .matches(path)
            .into_iter()
            .map(|glob| self.excluded_entry[glob])
            .collect();
        for (entry_index, by_name, pattern) in hits {
            if excluded.contains(&entry_index) {
                continue;
            }
            let entry = &self.entries[entry_index as usize];
            let rule = if by_name {
                let pattern = &entry.name_patterns[pattern as usize];
                format!("MAINTAINERS \"{}\" N: {pattern}", entry.title)
            } else {
                let pattern = &entry.files[pattern as usize];
                format!("MAINTAINERS \"{}\" F: {pattern}", entry.title)
            };
            for &(person, role) in &entry.people {
                out.push(OwnerMatch {
                    party: parties.get(person).clone(),
                    role,
                    component: Some(entry.component.clone()),
                    rule: rule.clone(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{OwnerRole, parse_maintainers};

    pub(crate) const SAMPLE: &str = "\
List of maintainers
===================

Descriptions of section entries and preferred order
---------------------------------------------------

\tM: *Mail* patches to: FullName <address@domain>
\tF: *Files* and directories wildcard patterns.
\t   F:\tdrivers/net/\tall files in and below drivers/net

Maintainers List
----------------

3C59X NETWORK DRIVER
M:\tSteffen Klassert <klassert@kernel.org>
R:\tRita Reviewer <rita@example.org>
L:\tnetdev@vger.kernel.org (moderated for non-subscribers)
S:\tOdd Fixes
W:\thttps://example.org/3c59x
T:\tgit git://git.kernel.org/pub/scm/net.git
B:\thttps://bugs.example.org
F:\tDocumentation/networking/vortex.rst
F:\tdrivers/net/ethernet/3com/
X:\tdrivers/net/ethernet/3com/legacy/
K:\tvortex_[a-z]+

NETWORKING DRIVERS
M:\tDavid Miller <davem@davemloft.net>
L:\tnetdev@vger.kernel.org
S:\tMaintained
F:\tdrivers/net/
F:\tinclude/linux/net*.h
N:\t(^|/)phy[^/]*\\.c$
N:\t(?=unsupported)

THE REST
M:\tLinus Torvalds <torvalds@linux-foundation.org>
S:\tBuried alive in reporters
F:\t*
F:\t*/
";

    fn titles(path: &str) -> Vec<String> {
        let rules = parse_maintainers(SAMPLE).unwrap();
        let mut titles: Vec<String> = rules
            .owners_of(path)
            .into_iter()
            .filter_map(|m| m.component.map(|key| key.to_string()))
            .collect();
        titles.dedup();
        titles
    }

    #[test]
    fn all_matching_entries_apply_with_f_x_and_n() {
        assert_eq!(
            titles("drivers/net/ethernet/3com/3c59x.c"),
            [
                "component:3c59x-network-driver",
                "component:networking-drivers",
                "component:the-rest"
            ]
        );
        // X: removes the entry for the excluded subtree only.
        assert_eq!(
            titles("drivers/net/ethernet/3com/legacy/old.c"),
            ["component:networking-drivers", "component:the-rest"]
        );
        // `*` does not cross directories; `?`-free file patterns are exact.
        assert_eq!(
            titles("include/linux/netdevice.h"),
            ["component:networking-drivers", "component:the-rest"]
        );
        assert_eq!(titles("include/linux/net/x.h"), ["component:the-rest"]);
        // N: is a regular expression on the path.
        assert_eq!(
            titles("arch/arm/phy-core.c"),
            ["component:networking-drivers", "component:the-rest"]
        );
        assert_eq!(titles("Makefile"), ["component:the-rest"]);
    }

    #[test]
    fn every_line_type_is_parsed_and_the_preamble_skipped() {
        let rules = parse_maintainers(SAMPLE).unwrap();
        let hits = rules.owners_of("Documentation/networking/vortex.rst");
        let first: Vec<_> = hits
            .iter()
            .filter(|m| m.rule.contains("3C59X"))
            .map(|m| (m.party.key.as_str(), m.role))
            .collect();
        assert_eq!(
            first,
            [
                ("person:klassert", OwnerRole::Maintainer),
                ("person:rita", OwnerRole::Reviewer),
                ("team:netdev-vger-kernel-org", OwnerRole::List),
            ]
        );
        assert!(
            hits[0]
                .rule
                .contains("F: Documentation/networking/vortex.rst")
        );
        // The unsupported look-ahead is reported, nothing else.
        assert_eq!(rules.warnings().len(), 1);
        assert!(rules.warnings()[0].message.contains("(?=unsupported)"));

        let unit = rules.definitions_unit(&"linux".into(), "MAINTAINERS");
        let components: Vec<_> = unit
            .nodes
            .iter()
            .filter(|n| n.key.as_str().starts_with("component:"))
            .collect();
        assert_eq!(components.len(), 3, "preamble must not produce entries");
        let c = components[0];
        assert_eq!(c.name, "3C59X NETWORK DRIVER");
        assert_eq!(c.attrs["status"], "Odd Fixes");
        assert_eq!(c.attrs["url"], "https://example.org/3c59x");
        assert_eq!(
            c.attrs["tree"][0],
            "git git://git.kernel.org/pub/scm/net.git"
        );
        assert_eq!(c.attrs["bugs"][0], "https://bugs.example.org");
        assert_eq!(c.attrs["patterns"].as_array().unwrap().len(), 2);
        assert_eq!(
            c.attrs["excluded_patterns"][0],
            "drivers/net/ethernet/3com/legacy/"
        );
        assert_eq!(c.attrs["keywords"][0], "vortex_[a-z]+");
    }
}
