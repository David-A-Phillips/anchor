//! Incremental Tree-sitter parsing and symbol extraction.
//!
//! * [`Language`] — the supported languages and their detection from a path;
//! * [`ParsedFile`] — symbols, references and imports extracted from one source file;
//! * [`IncrementalParser`] — a thread-safe parser with an LRU cache of syntax trees, so that a
//!   small edit to a file costs a small re-parse and yields a [`SymbolDelta`];
//! * [`build_file_unit`] — converts a parsed file into the ingestion [`Unit`](crate::base::Unit).
//!
//! Parsing is CPU-bound: call it from a blocking thread, never from the async executor.

mod config;
mod language;
mod model;

pub use config::ParserConfig;
pub use language::Language;
pub use model::{Import, ParsedFile, Reference, ReferenceKind, Symbol, SyntaxError, Visibility};
