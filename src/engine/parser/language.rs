//! Supported programming languages.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A language the parser understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// Rust (`.rs`).
    Rust,
    /// Python (`.py`, `.pyi`).
    Python,
    /// JavaScript, including JSX (`.js`, `.mjs`, `.cjs`, `.jsx`).
    JavaScript,
    /// TypeScript (`.ts`, `.mts`, `.cts`).
    TypeScript,
    /// TypeScript with JSX (`.tsx`).
    Tsx,
    /// Go (`.go`).
    Go,
    /// Java (`.java`).
    Java,
    /// C (`.c`, `.h`).
    C,
}

impl Language {
    /// Every supported language.
    pub const ALL: [Language; 8] = [
        Language::Rust,
        Language::Python,
        Language::JavaScript,
        Language::TypeScript,
        Language::Tsx,
        Language::Go,
        Language::Java,
        Language::C,
    ];

    /// Detects the language from a file extension (case-insensitive). `None` for anything the
    /// parser does not support.
    pub fn from_path(path: impl AsRef<Path>) -> Option<Self> {
        let extension = path.as_ref().extension()?.to_str()?.to_ascii_lowercase();
        Some(match extension.as_str() {
            "rs" => Language::Rust,
            "py" | "pyi" => Language::Python,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "go" => Language::Go,
            "java" => Language::Java,
            "c" | "h" => Language::C,
            _ => return None,
        })
    }

    /// Stable lowercase name (`"rust"`, `"python"`, `"javascript"`, `"typescript"`, `"tsx"`,
    /// `"go"`, `"java"`, `"c"`); also the serialised form.
    pub const fn name(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::Go => "go",
            Language::Java => "java",
            Language::C => "c",
        }
    }

    /// Inverse of [`Self::name`].
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|language| language.name() == name)
    }

    /// Position in [`Self::ALL`]; indexes per-language tables.
    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    /// `true` for the JavaScript family, which shares one extraction logic.
    pub(crate) const fn is_js_family(self) -> bool {
        matches!(
            self,
            Language::JavaScript | Language::TypeScript | Language::Tsx
        )
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
