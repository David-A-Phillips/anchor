//! In-repository documentation files (Markdown, reStructuredText, plain text) as file units.
//!
//! [`parse_doc_file`] turns one documentation file into a [`Unit`] of kind [`UnitKind::File`]:
//!
//! * a File node with the key and aliases a code file gets (`keys::file`; aliases: the
//!   repository-relative path and the path without extension), attributes `path`, `language`
//!   (`"markdown"`, `"rst"` or `"text"`) and, when the file has a heading, `title`;
//! * `Key(keys::service(<source id>)) --Contains--> file` (the Service node itself is asserted by
//!   the repository scanner); a `README` at the repository root additionally
//!   `--Documents--> Key(keys::service(<source id>))`;
//! * one Text chunk per section piece (heading path on the first line, at most about
//!   [`text::DEFAULT_CHUNK_CHARS`] bytes, split at paragraph boundaries);
//! * `Mentions` edges for every candidate entity found in the text
//!   ([`mentions::mention_edges`]), and for links to other files:
//!   relative Markdown/RST links resolve against the file's directory and look up a File by its
//!   path; Sphinx `:doc:` references and toctree entries look up a File by its path without
//!   extension (weight 0.9, scoped to the same source).
//!
//! Formats by extension (case-insensitive): `.md`, `.mdx`, `.markdown` are Markdown (YAML front
//! matter is skipped); `.rst` is reStructuredText; `.adoc`/`.asciidoc` get best-effort `=`
//! headings; everything else (`.txt`, ...) is plain text with underlined headings.
//!
//! Parsing is a single pass per file and typically takes well under a millisecond; it is still
//! CPU-bound work and belongs on a blocking thread, not on the async executor.
//!
//! [`text`] holds the utilities shared with the wiki parser and available to every other ingest
//! parser: Markdown/RST sectioning, HTML to text, chunking and link resolution.

pub mod text;

use std::collections::BTreeSet;

use crate::base::{
    ChunkKind, EdgeAssertion, EdgeKind, Lookup, NodeAssertion, NodeKind, SourceId, Unit, UnitKind,
    attrs, keys,
};
use crate::engine::ingest::mentions;

use text::{DocLink, LinkKind, ParsedText};

/// The `language` attribute a documentation file gets, from its extension. `None` for files this
/// module does not treat as documentation (callers decide what to pass in; [`parse_doc_file`]
/// itself falls back to plain text).
pub fn doc_language(rel_path: &str) -> Option<&'static str> {
    let name = rel_path.rsplit(['/', '\\']).next().unwrap_or(rel_path);
    let (_, extension) = name.rsplit_once('.')?;
    match extension.to_ascii_lowercase().as_str() {
        "md" | "mdx" | "markdown" => Some("markdown"),
        "rst" => Some("rst"),
        "txt" | "adoc" | "asciidoc" => Some("text"),
        _ => None,
    }
}

/// Parses one documentation file of the repository `source` into a canonical file unit.
///
/// `rel_path` is the repository-relative path (any slash style, optional `./`); `text` is the file
/// content. Never fails: unknown extensions are read as plain text and malformed markup degrades
/// to prose. The unit key is `keys::unit(source, UnitKind::File, <normalised path>)`.
pub fn parse_doc_file(source: &SourceId, rel_path: &str, text: &str) -> Unit {
    let path = keys::normalize_path(rel_path);
    let language = doc_language(&path).unwrap_or("text");
    let lower = path.to_ascii_lowercase();
    let parsed = match language {
        "markdown" => {
            let (_, body) = text::split_front_matter(text);
            ParsedText {
                sections: text::markdown_sections(body),
                links: text::markdown_links(body),
            }
        }
        "rst" => text::rst_document(text),
        _ if lower.ends_with(".adoc") || lower.ends_with(".asciidoc") => {
            text::asciidoc_document(text)
        }
        _ => text::plain_document(text),
    };

    let mut unit = Unit::new(
        keys::unit(source, UnitKind::File, &path),
        source.clone(),
        UnitKind::File,
    );
    let title = parsed
        .sections
        .iter()
        .find_map(|section| section.heading_path.first())
        .filter(|title| !title.is_empty());
    let file = unit.add_node(
        NodeAssertion::new(NodeKind::File, keys::file(source, &path), path.as_str())
            .alias(path.as_str())
            .alias(text::strip_extension(&path))
            .attr(attrs::PATH, path.as_str())
            .attr(attrs::LANGUAGE, language)
            .attr_opt(attrs::TITLE, title.cloned()),
    );
    let service = keys::service(source.as_str());
    unit.add_edge(EdgeAssertion::new(&service, EdgeKind::Contains, &file));
    let stem = text::strip_extension(&path);
    if !path.contains('/') && stem.eq_ignore_ascii_case("readme") {
        unit.add_edge(EdgeAssertion::new(&file, EdgeKind::Documents, &service));
    }

    for chunk in text::chunk_sections(&parsed.sections, text::DEFAULT_CHUNK_CHARS) {
        unit.add_chunk(&file, ChunkKind::Text, chunk);
    }

    // Mention extraction sees the rewritten text (roles as code spans), headings included.
    let mut prose = String::with_capacity(text.len());
    for section in &parsed.sections {
        if let Some(heading) = section.heading_path.last() {
            prose.push_str(heading);
            prose.push('\n');
        }
        prose.push_str(&section.body);
        prose.push('\n');
    }
    for edge in mentions::mention_edges(&file, &prose) {
        unit.add_edge(edge);
    }
    for target in link_targets(&path, &parsed.links) {
        unit.add_edge(
            EdgeAssertion::new(
                &file,
                EdgeKind::Mentions,
                Lookup::new(&[NodeKind::File], target).in_source(source.clone()),
            )
            .weight(0.9),
        );
    }
    unit.finish()
}

/// File aliases the links of the document at `path` may refer to, deduplicated.
fn link_targets(path: &str, links: &[DocLink]) -> BTreeSet<String> {
    let mut targets = BTreeSet::new();
    for link in links {
        match link.kind {
            LinkKind::Path => {
                if let Some(resolved) = text::resolve_relative(path, &link.target) {
                    targets.insert(resolved);
                }
            }
            LinkKind::Doc => {
                let Some(resolved) = text::resolve_relative(path, &link.target) else {
                    continue;
                };
                let resolved = text::strip_extension(&resolved).to_owned();
                // Sphinx resolves absolute references against the documentation root, which is
                // usually the top-level directory of the linking file (`Documentation/`).
                if link.target.starts_with('/')
                    && let Some((root, _)) = path.split_once('/')
                {
                    targets.insert(format!("{root}/{resolved}"));
                }
                targets.insert(resolved);
            }
            LinkKind::WikiTitle => {}
        }
    }
    targets.remove(path);
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::NodeRef;

    fn source() -> SourceId {
        SourceId::new("payments-api")
    }

    fn lookups(unit: &Unit, kind: EdgeKind) -> Vec<(String, Vec<NodeKind>)> {
        unit.edges
            .iter()
            .filter(|edge| edge.kind == kind)
            .filter_map(|edge| match &edge.to {
                NodeRef::Lookup(lookup) => Some((lookup.text.clone(), lookup.kinds.clone())),
                NodeRef::Key(_) => None,
            })
            .collect()
    }

    fn has_lookup(unit: &Unit, text: &str, kind: NodeKind) -> bool {
        lookups(unit, EdgeKind::Mentions)
            .iter()
            .any(|(found, kinds)| found == text && kinds.contains(&kind))
    }

    #[test]
    fn root_readme_documents_the_service() {
        let unit = parse_doc_file(
            &source(),
            "./README.md",
            "---\nx: 1\n---\n# Payments API\n\nUses `Ledger::charge` and PAY-123. See [setup](docs/setup.md).\n",
        );
        assert_eq!(unit.kind, UnitKind::File);
        assert_eq!(unit.key.as_str(), "payments-api/file/README.md");
        let file = &unit.nodes[0];
        assert_eq!(file.key.as_str(), "file:payments-api:README.md");
        assert_eq!(file.aliases, ["README", "README.md"]);
        assert_eq!(file.attrs[attrs::LANGUAGE], "markdown");
        assert_eq!(file.attrs[attrs::TITLE], "Payments API");
        let service = NodeRef::Key(keys::service("payments-api"));
        assert!(unit.edges.iter().any(|e| e.kind == EdgeKind::Contains
            && e.from == service
            && e.to == NodeRef::Key(file.key.clone())));
        assert!(
            unit.edges
                .iter()
                .any(|e| e.kind == EdgeKind::Documents && e.to == service)
        );
        assert!(has_lookup(&unit, "Ledger::charge", NodeKind::Symbol));
        assert!(has_lookup(&unit, "PAY-123", NodeKind::Ticket));
        assert!(has_lookup(&unit, "docs/setup.md", NodeKind::File));
        assert_eq!(unit.chunks.len(), 1);
        assert!(unit.chunks[0].text.starts_with("Payments API\n\n"));
        assert!(!unit.chunks[0].text.contains("x: 1"));
    }

    #[test]
    fn nested_readme_does_not_document_the_service() {
        let unit = parse_doc_file(&source(), "docs/README.md", "# Docs\n\ntext\n");
        assert!(unit.edges.iter().all(|e| e.kind != EdgeKind::Documents));
    }

    #[test]
    fn rst_roles_become_symbol_and_file_lookups() {
        let text = "Slab\n====\n\nUse :c:func:`kmalloc()`, see :doc:`/core-api/memory` and \
                    :doc:`sibling`.\n\n.. kernel-doc:: mm/slab.c\n";
        let unit = parse_doc_file(&source(), "Documentation/mm/slab.rst", text);
        assert_eq!(unit.nodes[0].attrs[attrs::LANGUAGE], "rst");
        assert!(has_lookup(&unit, "kmalloc", NodeKind::Symbol));
        assert!(has_lookup(&unit, "mm/slab.c", NodeKind::File));
        assert!(has_lookup(
            &unit,
            "Documentation/mm/sibling",
            NodeKind::File
        ));
        assert!(has_lookup(
            &unit,
            "Documentation/core-api/memory",
            NodeKind::File
        ));
        assert!(has_lookup(&unit, "core-api/memory", NodeKind::File));
    }

    #[test]
    fn plain_text_and_unknown_extensions_are_ingested() {
        let unit = parse_doc_file(
            &source(),
            "notes/überblick.txt",
            "Über\n====\n\nGrüße 你好\n",
        );
        assert_eq!(unit.nodes[0].attrs[attrs::LANGUAGE], "text");
        assert_eq!(unit.chunks[0].text, "Über\n\nGrüße 你好");
        let adoc = parse_doc_file(&source(), "guide.adoc", "= Guide\n\nbody\n");
        assert_eq!(adoc.nodes[0].attrs[attrs::TITLE], "Guide");
        let empty = parse_doc_file(&source(), "LICENSE", "");
        assert!(empty.chunks.is_empty());
        assert_eq!(doc_language("src/lib.rs"), None);
    }

    #[test]
    fn parsing_is_deterministic_and_fast() {
        let mut text = String::from("Big document\n============\n\n");
        for i in 0..200 {
            text.push_str(&format!(
                "Section {i}\n-----------\n\nCall :c:func:`do_thing_{i}()` from ``drivers/x/y{i}.c`` \
                 when CONFIG_FOO is set. See :doc:`other{i}`.\n\n"
            ));
        }
        let first = parse_doc_file(&source(), "Documentation/big.rst", &text);
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let again = parse_doc_file(&source(), "Documentation/big.rst", &text);
            assert_eq!(
                again.content_hash().expect("hash"),
                first.content_hash().expect("hash")
            );
        }
        // 20 parses of a 25 kB file; the budget is a few milliseconds per file.
        assert!(
            started.elapsed().as_millis() < 2000,
            "{:?}",
            started.elapsed()
        );
        assert!(first.chunks.len() >= 15);
    }
}
