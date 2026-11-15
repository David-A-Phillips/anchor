//! What the parser extracts from one source file.

use std::ops::Range;

use crate::base::{ContentHash, SymbolKind};

use super::Language;

/// Visibility of a symbol, reduced to three levels that exist in every language.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Visibility {
    /// Visible everywhere (`pub`, `public`, exported, capitalised in Go, non-`static` in C).
    Public,
    /// Visible to a limited scope (`pub(crate)`, `protected`, package-private, `_name` in Python).
    Restricted,
    /// Visible only to the enclosing item or file.
    Private,
}

impl Visibility {
    /// Textual form stored in the `visibility` node attribute: `"public"`, `"crate"`,
    /// `"private"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Restricted => "crate",
            Visibility::Private => "private",
        }
    }
}

/// A definition found in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    /// Simple name (`charge`). `impl` blocks are named `impl Type` / `impl Trait for Type` so
    /// that they never compete with the type itself in name lookups.
    pub name: String,
    /// Container chain joined with `::` (`Ledger::charge`), unique within the file: repeated
    /// definitions get `~2`, `~3`, ... in source order.
    pub qualified_name: String,
    /// What kind of definition this is.
    pub kind: SymbolKind,
    /// Index (into [`ParsedFile::symbols`]) of the enclosing symbol.
    pub parent: Option<usize>,
    /// Byte range of the definition in the source text (leading doc comments excluded).
    pub byte_range: Range<usize>,
    /// First line, 1-based.
    pub line_start: u32,
    /// Last line, 1-based, inclusive.
    pub line_end: u32,
    /// One-line signature: whitespace collapsed, body stripped.
    pub signature: String,
    /// Leading doc comment or docstring with comment markers removed.
    pub doc: Option<String>,
    /// Visibility.
    pub visibility: Visibility,
    /// BLAKE3 of the source bytes in `byte_range`.
    pub content_hash: ContentHash,
}

/// What a [`Reference`] refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReferenceKind {
    /// A function or method call.
    Call,
    /// A use of a type (annotation, instantiation, base class).
    Type,
    /// A macro invocation.
    Macro,
}

/// A use of a name defined elsewhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    /// Simple name of the target (`charge`).
    pub name: String,
    /// Text in front of the name: the `Ledger` of `Ledger::charge()`, or `self`, `this`, a
    /// variable or a package. Only short, simple expressions are kept.
    pub qualifier: Option<String>,
    /// `true` when the qualifier is syntactically a path (`a::b::name`, `pkg.Type` in a type
    /// position) rather than a receiver expression (`value.name()`).
    pub qualifier_is_path: bool,
    /// Kind of use.
    pub kind: ReferenceKind,
    /// Index of the innermost enclosing symbol, `None` at file level.
    pub from_symbol: Option<usize>,
    /// Line of the reference, 1-based.
    pub line: u32,
}

/// An import / include / use declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    /// The imported path as written, without quotes (`./ledger`, `a.b`, `crate::a::{B, C}`,
    /// `stdio.h`).
    pub path: String,
    /// Imported names when the syntax lists them (original names, not local aliases).
    pub names: Vec<String>,
    /// `true` when the path is relative to the importing file or its package (`./x`, Python
    /// `.x`, C `"x.h"`, Rust `crate::`/`self::`/`super::`).
    pub relative: bool,
    /// Line of the declaration, 1-based.
    pub line: u32,
}

/// A syntax error Tree-sitter recovered from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxError {
    /// Line, 1-based.
    pub line: u32,
    /// Column, 1-based, in bytes.
    pub column: u32,
    /// Byte range of the offending or missing node.
    pub byte_range: Range<usize>,
    /// Short description.
    pub message: String,
}

/// Everything extracted from one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedFile {
    /// Language the file was parsed as.
    pub language: Language,
    /// Definitions in source order (a lexical parent always precedes its children).
    pub symbols: Vec<Symbol>,
    /// Call sites, type uses and macro invocations in source order.
    pub references: Vec<Reference>,
    /// Imports in source order.
    pub imports: Vec<Import>,
    /// Syntax errors (at most [`ParsedFile::MAX_SYNTAX_ERRORS`] are reported).
    pub syntax_errors: Vec<SyntaxError>,
    /// File-level documentation (`//!`, module docstring, leading comment block).
    pub module_doc: Option<String>,
}

impl ParsedFile {
    /// Upper bound on reported syntax errors; a badly broken file does not need thousands.
    pub const MAX_SYNTAX_ERRORS: usize = 64;

    /// A file with nothing in it.
    pub fn empty(language: Language) -> Self {
        Self {
            language,
            symbols: Vec::new(),
            references: Vec::new(),
            imports: Vec::new(),
            syntax_errors: Vec::new(),
            module_doc: None,
        }
    }

    /// Finds a symbol by its qualified name.
    pub fn symbol(&self, qualified_name: &str) -> Option<&Symbol> {
        self.symbols
            .iter()
            .find(|symbol| symbol.qualified_name == qualified_name)
    }
}
