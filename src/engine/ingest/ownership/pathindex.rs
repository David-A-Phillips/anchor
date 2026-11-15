//! A set of path globs indexed by their literal prefix.
//!
//! Ownership files hold thousands of patterns, almost all of which start with a literal path
//! (`drivers/net/**`, `include/linux/can*.h`). Testing every pattern against a path is too slow,
//! and a single `GlobSet` degrades to a linear scan for patterns sharing an extension. Here each
//! pattern is filed under the text before its first wildcard; a lookup probes the prefixes of the
//! queried path (one hash lookup per distinct prefix length in the set) and verifies only the
//! patterns found there. Patterns without a literal prefix (`**/*.js`) go to a `GlobSet`.

use std::collections::HashMap;

use globset::{Error, GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};

enum Verify {
    /// The pattern is `<prefix>**` with the prefix ending in `/`: any path below it matches.
    Subtree,
    /// The pattern is a literal path.
    Exact,
    /// Anything else is confirmed by the compiled glob.
    Glob(Box<GlobMatcher>),
}

/// Builder for a [`PathIndex`].
pub(super) struct PathIndexBuilder {
    buckets: HashMap<String, Vec<(Verify, u32)>>,
    floating: GlobSetBuilder,
    floating_ids: Vec<u32>,
}

impl Default for PathIndexBuilder {
    fn default() -> Self {
        Self {
            buckets: HashMap::new(),
            floating: GlobSetBuilder::new(),
            floating_ids: Vec::new(),
        }
    }
}

/// Compiled glob set; see the module documentation.
pub(super) struct PathIndex {
    buckets: HashMap<String, Vec<(Verify, u32)>>,
    /// Distinct byte lengths of the bucket keys, ascending.
    lengths: Vec<usize>,
    floating: GlobSet,
    floating_ids: Vec<u32>,
}

impl PathIndexBuilder {
    /// Adds a glob (globset syntax, `*` and `?` never crossing `/`) reporting `id` on a match.
    /// With `backslash_escape` a backslash quotes the next character, otherwise it is literal.
    pub(super) fn add(&mut self, glob: &str, backslash_escape: bool, id: u32) -> Result<(), Error> {
        let compiled = GlobBuilder::new(glob)
            .literal_separator(true)
            .backslash_escape(backslash_escape)
            .build()?;
        let is_meta =
            |c: char| matches!(c, '*' | '?' | '[' | '{') || (backslash_escape && c == '\\');
        let (prefix, verify) = match glob.find(is_meta) {
            None => (glob, Verify::Exact),
            Some(0) => {
                self.floating.add(compiled);
                self.floating_ids.push(id);
                return Ok(());
            }
            Some(at) if &glob[at..] == "**" && glob[..at].ends_with('/') => {
                (&glob[..at], Verify::Subtree)
            }
            Some(at) => (
                &glob[..at],
                Verify::Glob(Box::new(compiled.compile_matcher())),
            ),
        };
        self.buckets
            .entry(prefix.to_owned())
            .or_default()
            .push((verify, id));
        Ok(())
    }

    pub(super) fn build(self) -> Result<PathIndex, Error> {
        let mut lengths: Vec<usize> = self.buckets.keys().map(String::len).collect();
        lengths.sort_unstable();
        lengths.dedup();
        Ok(PathIndex {
            buckets: self.buckets,
            lengths,
            floating: self.floating.build()?,
            floating_ids: self.floating_ids,
        })
    }
}

impl PathIndex {
    /// Appends the ids of the globs matching `path`, in no particular order; an id is reported
    /// once per matching glob registered with it.
    pub(super) fn matches(&self, path: &str, out: &mut Vec<u32>) {
        for &length in &self.lengths {
            if length > path.len() {
                break;
            }
            let Some(bucket) = path.get(..length).and_then(|key| self.buckets.get(key)) else {
                continue;
            };
            for (verify, id) in bucket {
                let hit = match verify {
                    Verify::Subtree => true,
                    Verify::Exact => path.len() == length,
                    Verify::Glob(matcher) => matcher.is_match(path),
                };
                if hit {
                    out.push(*id);
                }
            }
        }
        if !self.floating_ids.is_empty() {
            out.extend(
                self.floating
                    .matches(path)
                    .into_iter()
                    .map(|glob| self.floating_ids[glob]),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_strategy_agrees_with_plain_glob_matching() {
        let globs = [
            "docs/**",
            "docs/readme.md",
            "docs/*.md",
            "src/gen?/[ab]*.rs",
            "**/*.js",
            "**",
            "esc/\\*lit",
        ];
        let mut builder = PathIndexBuilder::default();
        for (id, glob) in globs.iter().enumerate() {
            builder.add(glob, true, id as u32).unwrap();
        }
        assert!(builder.add("[z-a]", true, 99).is_err());
        let index = builder.build().unwrap();
        for path in [
            "docs/readme.md",
            "docs/a/b.md",
            "docs",
            "src/gen1/a.rs",
            "src/gen1/c.rs",
            "x/y/z.js",
            "esc/*lit",
            "esc/xlit",
            "dócs/ü.md",
        ] {
            let mut found = Vec::new();
            index.matches(path, &mut found);
            found.sort_unstable();
            let expected: Vec<u32> = globs
                .iter()
                .enumerate()
                .filter(|(_, glob)| {
                    GlobBuilder::new(glob)
                        .literal_separator(true)
                        .backslash_escape(true)
                        .build()
                        .unwrap()
                        .compile_matcher()
                        .is_match(path)
                })
                .map(|(id, _)| id as u32)
                .collect();
            assert_eq!(found, expected, "{path}");
        }
    }
}
