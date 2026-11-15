//! Streaming file output with per-kind byte accounting.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::base::Result;

/// Writes files under the output directory and counts what it wrote per source kind.
pub(crate) struct Writer {
    root: PathBuf,
    bytes: BTreeMap<String, u64>,
    files: BTreeMap<String, usize>,
}

impl Writer {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            bytes: BTreeMap::new(),
            files: BTreeMap::new(),
        }
    }

    /// Writes `content` to `rel` (created with its parent directories) and accounts it to `kind`.
    pub(crate) fn write(&mut self, kind: &str, rel: &str, content: &str) -> Result<()> {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        *self.bytes.entry(kind.to_owned()).or_insert(0) += content.len() as u64;
        *self.files.entry(kind.to_owned()).or_insert(0) += 1;
        Ok(())
    }

    pub(crate) fn finish(self) -> (BTreeMap<String, u64>, BTreeMap<String, usize>) {
        (self.bytes, self.files)
    }
}
