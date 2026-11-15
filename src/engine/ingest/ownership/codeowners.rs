//! CODEOWNERS (GitHub and GitLab flavours).
//!
//! Each line is a gitignore-style pattern followed by owners. Within a section the **last**
//! matching pattern decides; a pattern without owners clears ownership (or, in a GitLab section
//! with default owners, inherits the defaults). GitLab `[Section]` headers split the file into
//! independent rule lists: every section contributes its own last match.

use globset::GlobBuilder;

use super::pathindex::{PathIndex, PathIndexBuilder};
use super::{OwnerMatch, OwnerRole, Parties};
use crate::base::{Error, Result};
use crate::engine::ingest::{IngestWarning, party};

struct Rule {
    owners: Vec<usize>,
    section: u32,
    description: String,
}

/// Compiled CODEOWNERS rules.
pub(super) struct Compiled {
    rules: Vec<Rule>,
    /// Reports rule indices (a pattern may compile to two globs).
    index: PathIndex,
}

struct Section {
    name: String,
    approvals: Option<u32>,
    defaults: Vec<usize>,
}

/// Splits a line into tokens on unescaped whitespace, dropping a trailing `# comment`.
/// `\ ` and `\#` become literal characters; other escapes are left for the glob compiler.
fn tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some(&next @ (' ' | '#')) => {
                    current.push(next);
                    chars.next();
                }
                Some(&next) => {
                    current.push('\\');
                    current.push(next);
                    chars.next();
                }
                None => current.push('\\'),
            },
            '#' if current.is_empty() => break,
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Parses a GitLab section header: `[Name]`, `^[Name]`, `[Name][2]`, each optionally followed by
/// default owners. Returns `(name, approvals, rest of the line)`.
fn section_header(line: &str) -> Option<(&str, Option<u32>, &str)> {
    let body = line.strip_prefix('^').unwrap_or(line).strip_prefix('[')?;
    let close = body.find(']')?;
    let name = body[..close].trim();
    let mut rest = &body[close + 1..];
    let mut approvals = None;
    if let Some(inner) = rest.strip_prefix('[')
        && let Some(end) = inner.find(']')
        && let Ok(count) = inner[..end].trim().parse::<u32>()
    {
        approvals = Some(count);
        rest = &inner[end + 1..];
    }
    // `[abc]*.js @owner` is a character-class pattern, not a section.
    let is_header = !name.is_empty() && (rest.is_empty() || rest.starts_with(char::is_whitespace));
    is_header.then_some((name, approvals, rest))
}

fn has_glob_meta(segment: &str) -> bool {
    segment.contains(['*', '?', '[', '{', '\\'])
}

/// Translates a gitignore-style pattern into globs over repository-relative paths.
///
/// A leading or inner slash anchors the pattern at the root, otherwise it matches at any depth. A
/// trailing slash matches the directory's whole subtree. A pattern whose last segment is a plain
/// name matches both that file and, if it is a directory, everything below it; a last segment
/// with wildcards (`docs/*`) matches only at that level, as GitHub documents.
pub(super) fn globs_for(pattern: &str) -> Vec<String> {
    let directory = pattern.ends_with('/');
    let body = pattern.trim_end_matches('/');
    let anchored = body.contains('/');
    let body = body.trim_start_matches('/');
    if body.is_empty() {
        return vec!["**".to_owned()];
    }
    let base = if anchored || body.starts_with("**") {
        body.to_owned()
    } else {
        format!("**/{body}")
    };
    let last = body.rsplit('/').next().unwrap_or(body);
    if directory {
        vec![format!("{base}/**")]
    } else if has_glob_meta(last) {
        vec![base]
    } else {
        vec![format!("{base}/**"), base]
    }
}

pub(super) fn parse(
    text: &str,
    parties: &mut Parties,
    warnings: &mut Vec<IngestWarning>,
) -> Result<Compiled> {
    let mut sections = vec![Section {
        name: String::new(),
        approvals: None,
        defaults: Vec::new(),
    }];
    let mut current = 0usize;
    let mut rules = Vec::new();
    let mut builder = PathIndexBuilder::default();

    let mut owners_of = |tokens: &[String], line: usize, warnings: &mut Vec<IngestWarning>| {
        let mut owners = Vec::new();
        for token in tokens {
            // Owners are `@user`, `@org/team` or an e-mail address; anything else is a typo.
            match party::party_assertion(token).filter(|_| token.contains('@')) {
                Some(node) => owners.push(parties.intern(node)),
                None => warnings.push(IngestWarning::new(
                    format!("line {line}"),
                    format!("ignored invalid owner `{token}`"),
                )),
            }
        }
        owners
    };

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, approvals, rest)) = section_header(line) {
            let defaults = owners_of(&tokens(rest), line_no, warnings);
            // GitLab merges sections whose names differ only by case.
            current = match sections
                .iter()
                .position(|section| section.name.eq_ignore_ascii_case(name))
            {
                Some(existing) => existing,
                None => {
                    sections.push(Section {
                        name: name.to_owned(),
                        approvals: None,
                        defaults: Vec::new(),
                    });
                    sections.len() - 1
                }
            };
            let section = &mut sections[current];
            section.approvals = approvals.or(section.approvals);
            if !defaults.is_empty() {
                section.defaults = defaults;
            }
            continue;
        }
        let parts = tokens(line);
        let Some((pattern, owner_tokens)) = parts.split_first() else {
            continue;
        };
        let mut globs = Vec::new();
        let mut valid = true;
        for glob in globs_for(pattern) {
            // Validate before registering anything, so a rule is indexed entirely or not at all.
            match GlobBuilder::new(&glob)
                .literal_separator(true)
                .backslash_escape(true)
                .build()
            {
                Ok(_) => globs.push(glob),
                Err(err) => {
                    warnings.push(IngestWarning::new(
                        format!("line {line_no}"),
                        format!("ignored invalid pattern `{pattern}`: {err}"),
                    ));
                    valid = false;
                    break;
                }
            }
        }
        if !valid {
            continue;
        }
        let section = &sections[current];
        let mut owners = owners_of(owner_tokens, line_no, warnings);
        if owner_tokens.is_empty() {
            owners.clone_from(&section.defaults);
        }
        let mut description = format!("CODEOWNERS line {line_no}: {pattern}");
        if !section.name.is_empty() {
            description.push_str(&format!(" [{}]", section.name));
            if let Some(count) = section.approvals {
                description.push_str(&format!(" ({count} approvals)"));
            }
        }
        let rule = u32::try_from(rules.len())
            .map_err(|_| Error::invalid("CODEOWNERS file has too many rules"))?;
        for glob in &globs {
            builder
                .add(glob, true, rule)
                .map_err(|err| Error::parse(format!("CODEOWNERS line {line_no}: {err}")))?;
        }
        rules.push(Rule {
            owners,
            section: current as u32,
            description,
        });
    }
    let index = builder
        .build()
        .map_err(|err| Error::parse(format!("CODEOWNERS patterns: {err}")))?;
    Ok(Compiled { rules, index })
}

impl Compiled {
    pub(super) fn matches(&self, path: &str, parties: &Parties, out: &mut Vec<OwnerMatch>) {
        // Last matching rule per section; files rarely have more than a handful of sections, so
        // a linear scan of this small vector beats a map.
        let mut winners: Vec<(u32, u32)> = Vec::new();
        let mut hits = Vec::new();
        self.index.matches(path, &mut hits);
        for rule in hits {
            let section = self.rules[rule as usize].section;
            match winners.iter_mut().find(|(s, _)| *s == section) {
                Some(winner) => winner.1 = winner.1.max(rule),
                None => winners.push((section, rule)),
            }
        }
        winners.sort_unstable();
        for (_, rule) in winners {
            let rule = &self.rules[rule as usize];
            for &owner in &rule.owners {
                out.push(OwnerMatch {
                    party: parties.get(owner).clone(),
                    role: OwnerRole::Owner,
                    component: None,
                    rule: rule.description.clone(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse_codeowners;

    fn owners(text: &str, path: &str) -> Vec<String> {
        let rules = parse_codeowners(text).unwrap();
        rules
            .owners_of(path)
            .into_iter()
            .map(|m| m.party.key.to_string())
            .collect()
    }

    const GITHUB: &str = "\
# global owners
*       @global
*.js    @js-owner  # inline comment
/build/logs/ @doctocat
docs/*  docs@example.com
apps/   @octocat
/docs/  @acme/doc-team
/src/**/gen/*.rs @generator
/apps/github
with\\ space/ @spacey
";

    #[test]
    fn last_match_wins_with_anchoring_directories_and_globs() {
        assert_eq!(owners(GITHUB, "README.md"), ["person:global"]);
        assert_eq!(owners(GITHUB, "lib/deep/x.js"), ["person:js-owner"]);
        assert_eq!(owners(GITHUB, "build/logs/a/b.log"), ["person:doctocat"]);
        // `/build/logs/` is anchored: a nested build/logs falls back to `*`.
        assert_eq!(owners(GITHUB, "x/build/logs/b.log"), ["person:global"]);
        // `docs/*` matches direct children only, and only at the root (inner slash anchors).
        assert_eq!(owners(GITHUB, "other/docs/a.md"), ["person:global"]);
        // `/docs/` comes later and owns the whole tree, including direct children.
        assert_eq!(owners(GITHUB, "docs/a.md"), ["team:doc-team"]);
        assert_eq!(owners(GITHUB, "docs/sub/a.md"), ["team:doc-team"]);
        // `apps/` is unanchored: any directory named apps.
        assert_eq!(owners(GITHUB, "x/apps/y/z.rb"), ["person:octocat"]);
        assert_eq!(owners(GITHUB, "src/a/b/gen/x.rs"), ["person:generator"]);
        assert_eq!(owners(GITHUB, "src/gen/x.rs"), ["person:generator"]);
        assert_eq!(owners(GITHUB, "with space/f"), ["person:spacey"]);
        // Leading slashes and `./` in the queried path are normalised.
        assert_eq!(owners(GITHUB, "/lib/x.js"), ["person:js-owner"]);
    }

    #[test]
    fn pattern_without_owners_clears_ownership() {
        assert!(owners(GITHUB, "apps/github/main.rb").is_empty());
        assert_eq!(owners(GITHUB, "apps/other/main.rb"), ["person:octocat"]);
    }

    #[test]
    fn gitlab_sections_each_contribute_and_inherit_defaults() {
        let text = "\
* @default

[Backend][2] @be-lead
*.rb
/app/models/ @acme/models

^[Docs]
*.md @writer
[backend]
/legacy/
[abc]*.txt @charclass
";
        let rules = parse_codeowners(text).unwrap();
        let hits = rules.owners_of("app/models/user.rb");
        let keys: Vec<_> = hits.iter().map(|m| m.party.key.as_str()).collect();
        assert_eq!(keys, ["person:default", "team:models"]);
        assert!(hits[1].rule.contains("[Backend]") && hits[1].rule.contains("2 approvals"));
        // Section default owners apply to an owner-less pattern.
        assert_eq!(
            owners(text, "lib/x.rb"),
            ["person:default", "person:be-lead"]
        );
        assert_eq!(
            owners(text, "README.md"),
            ["person:default", "person:writer"]
        );
        // `[backend]` re-opens Backend (case-insensitive); `/legacy/` inherits its defaults.
        assert_eq!(
            owners(text, "legacy/x.c"),
            ["person:default", "person:be-lead"]
        );
        // A bracket expression followed by pattern text is a pattern, not a section.
        assert_eq!(
            owners(text, "a.txt"),
            ["person:default", "person:charclass"]
        );
    }

    #[test]
    fn invalid_owners_and_patterns_are_reported_not_fatal() {
        let rules = parse_codeowners("a/ notanowner @ok\n[z-a].txt @x\n").unwrap();
        assert_eq!(rules.warnings().len(), 2);
        assert_eq!(rules.owners_of("a/b").len(), 1);
    }
}
