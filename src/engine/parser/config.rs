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
    /// Extract call, reference and import edges from code. Turning this off keeps only files and
    /// symbols, which shrinks the database considerably for very large repositories.
    pub call_graph: bool,
    /// Keep references that resolve to nothing after a scan (calls into standard libraries and
    /// third-party code) so they can link if the target is ingested later. Off by default: such
    /// references are the largest single contributor to database size.
    pub keep_unresolved_references: bool,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: 1024 * 1024,
            tree_cache_entries: 256,
            call_graph: true,
            keep_unresolved_references: false,
        }
    }
}
