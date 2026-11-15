//! Parser configuration (`[parser]`).

use serde::{Deserialize, Serialize};

/// Limits and cache sizes of the code parser.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ParserConfig {
    /// Files larger than this many bytes are recorded but not parsed.
    pub max_file_bytes: u64,
    /// Number of syntax trees kept for incremental re-parsing of recently changed files.
    pub tree_cache_entries: usize,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: 1024 * 1024,
            tree_cache_entries: 256,
        }
    }
}
