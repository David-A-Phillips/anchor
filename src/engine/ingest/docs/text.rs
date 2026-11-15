//! Text utilities shared by the documentation, wiki and other ingest parsers.
//!
//! * [`markdown_sections`], [`rst_document`], [`plain_document`], [`asciidoc_document`] split a
//!   document into [`Section`]s at headings;
//! * [`html_to_text`] / [`element_text`] reduce HTML to Markdown-flavoured plain text (headings as
//!   `#` lines, code as backtick spans, so that [`markdown_sections`] and mention extraction work
//!   on the result);
//! * [`chunk_sections`] / [`split_text`] cut sections into bounded chunks at paragraph boundaries;
//! * [`markdown_links`] and [`resolve_relative`] find and normalise links between documents.
//!
//! Everything here is a single pass over the input with no allocation proportional to anything but
//! the output: documentation trees with tens of thousands of files go through these functions.

use std::sync::LazyLock;

use regex::{Captures, Regex};
use scraper::{ElementRef, Html, Node};

/// Default upper bound of a text chunk, in bytes of UTF-8 (about 1500 characters of prose).
pub const DEFAULT_CHUNK_CHARS: usize = 1500;

/// Nesting depth beyond which HTML is flattened instead of walked recursively, so that hostile or
/// broken markup cannot exhaust the stack.
const MAX_HTML_DEPTH: usize = 200;

/// A run of text under one heading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// Headings from the outermost down to the one directly above the text; empty for text that
    /// precedes the first heading.
    pub heading_path: Vec<String>,
    /// Level of the innermost heading (1 = top), 0 for text before the first heading.
    pub level: u8,
    /// The text of the section without its heading line; may be empty for a heading that is
    /// directly followed by a sub-heading.
    pub body: String,
}

/// What a link found in a document points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// A URL or relative path (`[text](../other.md)`, `<a href>`, RST `` `text <path>`_ ``).
    Path,
    /// A wiki-style title link (`[[Page Title]]`).
    WikiTitle,
    /// A Sphinx document reference without extension (`:doc:`, toctree entries).
    Doc,
}

/// A link from one document to another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocLink {
    /// What kind of target this is.
    pub kind: LinkKind,
    /// The target exactly as written (path, URL or title).
    pub target: String,
}

/// Sections and outgoing links of a parsed document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedText {
    /// Sections in document order.
    pub sections: Vec<Section>,
    /// Links in document order (not deduplicated).
    pub links: Vec<DocLink>,
}

#[derive(Default)]
struct SectionBuilder {
    sections: Vec<Section>,
    path: Vec<(u8, String)>,
    body: String,
}

impl SectionBuilder {
    fn heading(&mut self, level: u8, title: &str) {
        self.flush();
        while self.path.last().is_some_and(|(open, _)| *open >= level) {
            self.path.pop();
        }
        self.path.push((level, title.trim().to_owned()));
    }

    fn line(&mut self, line: &str) {
        self.body.push_str(line.trim_end());
        self.body.push('\n');
    }

    fn flush(&mut self) {
        let body = self.body.trim_matches('\n').trim_end();
        if !body.is_empty() || !self.path.is_empty() {
            self.sections.push(Section {
                heading_path: self.path.iter().map(|(_, title)| title.clone()).collect(),
                level: self.path.last().map_or(0, |(level, _)| *level),
                body: body.to_owned(),
            });
        }
        self.body.clear();
    }

    fn finish(mut self) -> Vec<Section> {
        self.flush();
        self.sections
    }
}

/// Splits YAML front matter (a block delimited by `---` lines at the very start of the text,
/// closed by `---` or `...`) from the rest. Returns `(None, text)` when there is none or it is
/// not closed.
pub fn split_front_matter(text: &str) -> (Option<&str>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(first_end) = text.find('\n') else {
        return (None, text);
    };
    if text[..first_end].trim_end() != "---" {
        return (None, text);
    }
    let mut offset = first_end + 1;
    for line in text[offset..].split_inclusive('\n') {
        let marker = line.trim_end();
        if marker == "---" || marker == "..." {
            return (
                Some(&text[first_end + 1..offset]),
                &text[offset + line.len()..],
            );
        }
        offset += line.len();
    }
    (None, text)
}

fn fence_marker(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let marker = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let length = trimmed.chars().take_while(|c| *c == marker).count();
    (length >= 3).then_some((marker, length))
}

fn atx_heading(line: &str) -> Option<(u8, &str)> {
    let trimmed = line.trim_start();
    let level = trimmed.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    Some((level as u8, title))
}

fn setext_level(line: &str) -> Option<u8> {
    let trimmed = line.trim();
    if trimmed.len() < 2 {
        return None;
    }
    if trimmed.bytes().all(|b| b == b'=') {
        Some(1)
    } else if trimmed.bytes().all(|b| b == b'-') {
        Some(2)
    } else {
        None
    }
}

/// Splits Markdown into sections at ATX (`# Title`) and setext (underlined) headings. Fenced code
/// blocks are never mistaken for headings. Section bodies keep the Markdown source verbatim (code
/// spans stay in backticks, which is what mention extraction looks for). Front matter must be
/// removed first with [`split_front_matter`].
pub fn markdown_sections(text: &str) -> Vec<Section> {
    let mut builder = SectionBuilder::default();
    let mut fence: Option<(char, usize)> = None;
    // A paragraph line that becomes a setext heading if the next line underlines it.
    let mut pending: Option<&str> = None;
    for line in text.lines() {
        if let Some((marker, length)) = fence {
            builder.line(line);
            if fence_marker(line).is_some_and(|(m, l)| m == marker && l >= length) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = fence_marker(line) {
            if let Some(previous) = pending.take() {
                builder.line(previous);
            }
            builder.line(line);
            fence = Some(open);
            continue;
        }
        if let Some(title) = pending.take() {
            if let Some(level) = setext_level(line) {
                builder.heading(level, title);
                continue;
            }
            builder.line(title);
        }
        if let Some((level, title)) = atx_heading(line) {
            builder.heading(level, title);
        } else if line.trim().is_empty() || line.trim_start().starts_with(['-', '*', '>', '|', '+'])
        {
            builder.line(line);
        } else {
            pending = Some(line);
        }
    }
    if let Some(previous) = pending {
        builder.line(previous);
    }
    builder.finish()
}

static MARKDOWN_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\[\[([^\]\n]+)\]\]|\[[^\]\n]*\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"\n]*")?\s*\)"#)
        .expect("the Markdown link pattern is a valid regex")
});

/// Links in Markdown text: inline links `[text](target)` (images are skipped) as
/// [`LinkKind::Path`], and wiki links `[[Title]]`, `[[Title|label]]`, `[[Title#section]]` as
/// [`LinkKind::WikiTitle`] (label and section removed).
pub fn markdown_links(text: &str) -> Vec<DocLink> {
    if !text.contains('[') {
        return Vec::new();
    }
    let mut links = Vec::new();
    for caps in MARKDOWN_LINK.captures_iter(text) {
        if let Some(title) = caps.get(1) {
            let title = title.as_str();
            let title = title.split(['|', '#']).next().unwrap_or(title).trim();
            if !title.is_empty() {
                links.push(DocLink {
                    kind: LinkKind::WikiTitle,
                    target: title.to_owned(),
                });
            }
        } else if let (Some(whole), Some(target)) = (caps.get(0), caps.get(2))
            && !text[..whole.start()].ends_with('!')
        {
            links.push(DocLink {
                kind: LinkKind::Path,
                target: target.as_str().to_owned(),
            });
        }
    }
    links
}

/// Resolves a link target written in the document at `base_file` (a root-relative path) to a
/// normalised root-relative path. Fragments and query strings are removed, `%20` is decoded, `.`
/// and `..` are folded, and a leading `/` means the root. Returns `None` for external links
/// (anything with a scheme, `mailto:`, protocol-relative URLs) and pure fragments.
pub fn resolve_relative(base_file: &str, target: &str) -> Option<String> {
    let target = target.trim();
    let target = target.split(['#', '?']).next().unwrap_or(target);
    if target.is_empty() || target.starts_with("//") {
        return None;
    }
    if let Some(colon) = target.find(':') {
        // `scheme:...`; a colon after the first slash is part of a path.
        if !target[..colon].contains('/') {
            return None;
        }
    }
    let target = target.replace("%20", " ").replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    if !target.starts_with('/') {
        let base = base_file.replace('\\', "/");
        let directory = base.rsplit_once('/').map_or("", |(directory, _)| directory);
        let owned: Vec<String> = directory
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .map(str::to_owned)
            .collect();
        return Some(fold_path(owned, &target)).filter(|path| !path.is_empty());
    }
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/")).filter(|path| !path.is_empty())
}

fn fold_path(mut parts: Vec<String>, relative: &str) -> String {
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_owned()),
        }
    }
    parts.join("/")
}

/// The path without its final extension (`docs/guide.md` → `docs/guide`); dots in directory names
/// and leading dots of hidden files are left alone.
pub fn strip_extension(path: &str) -> &str {
    let name_start = path.rfind('/').map_or(0, |slash| slash + 1);
    match path[name_start..].rfind('.') {
        Some(dot) if dot > 0 => &path[..name_start + dot],
        _ => path,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavour {
    Rst,
    Plain,
    AsciiDoc,
}

/// Parses reStructuredText: headings are detected from adornment lines (underline, or overline
/// plus underline; levels are assigned in order of first appearance of each adornment style),
/// inline roles and literals are rewritten so that mention extraction sees them
/// (``:c:func:`foo()` `` and ```` ``foo`` ```` become `` `foo` ``, `:doc:` and toctree entries
/// become [`LinkKind::Doc`] links, `:ref:` keeps its title), and directive noise (directive
/// markers, options, comments, link targets, images, toctrees, raw blocks) is dropped while the
/// content of admonitions and code blocks is kept.
pub fn rst_document(text: &str) -> ParsedText {
    underlined_document(text, Flavour::Rst)
}

/// Parses plain text: headings are lines underlined with punctuation (as in RFC-style and
/// kernel-style `.txt` files); everything else is kept verbatim.
pub fn plain_document(text: &str) -> ParsedText {
    underlined_document(text, Flavour::Plain)
}

/// Parses AsciiDoc on a best-effort basis: `= Title` / `== Section` headings; everything else is
/// kept verbatim.
pub fn asciidoc_document(text: &str) -> ParsedText {
    underlined_document(text, Flavour::AsciiDoc)
}

fn adornment(line: &str) -> Option<char> {
    let trimmed = line.trim_end();
    let mut chars = trimmed.chars();
    let first = chars.next().filter(char::is_ascii_punctuation)?;
    (trimmed.len() >= 3 && chars.all(|c| c == first)).then_some(first)
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Index of the first line after the indented block that follows line `start`.
fn block_end(lines: &[&str], start: usize) -> usize {
    let indent = indent_of(lines[start]);
    let mut end = start + 1;
    let mut last_content = start + 1;
    while end < lines.len() {
        let line = lines[end];
        if !line.trim().is_empty() {
            if indent_of(line) <= indent {
                break;
            }
            last_content = end + 1;
        }
        end += 1;
    }
    last_content
}

/// Directives whose whole block is noise for retrieval.
const RST_DROPPED: &[&str] = &[
    "image",
    "figure",
    "kernel-figure",
    "kernel-render",
    "raw",
    "contents",
    "sectnum",
    "highlight",
    "index",
    "tabularcolumns",
    "meta",
    "role",
    "default-role",
    "title",
    "only",
    "include",
    "literalinclude",
    "kernel-doc",
    "automodule",
    "autoclass",
    "autofunction",
];

/// Directives whose argument names a file or symbol worth linking.
const RST_ARGUMENT_IS_REFERENCE: &[&str] = &[
    "include",
    "literalinclude",
    "kernel-doc",
    "automodule",
    "autoclass",
    "autofunction",
];

/// Directives whose argument is a language or other non-prose token.
const RST_ARGUMENT_IS_NOISE: &[&str] = &["code-block", "code", "sourcecode", "parsed-literal"];

static RST_INLINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
          :(?P<role>[A-Za-z][A-Za-z0-9_:.+-]*):`(?P<body>[^`]+)`
        | ``(?P<lit>[^`]+)``
        | `(?P<text>[^`<]+?)\s*<(?P<url>[^`>]+)>`__?",
    )
    .expect("the RST inline pattern is a valid regex")
});

/// Splits `Title <target>` into its parts; a body without angle brackets is only a target.
fn title_and_target(body: &str) -> (Option<&str>, &str) {
    let body = body.trim();
    if let Some(stripped) = body.strip_suffix('>')
        && let Some((title, target)) = stripped.rsplit_once('<')
    {
        let title = title.trim();
        return ((!title.is_empty()).then_some(title), target.trim());
    }
    (None, body)
}

fn rst_inline(line: &str, links: &mut Vec<DocLink>) -> String {
    RST_INLINE
        .replace_all(line, |caps: &Captures<'_>| -> String {
            if let Some(literal) = caps.name("lit") {
                return format!("`{}`", literal.as_str().trim());
            }
            if let (Some(text), Some(url)) = (caps.name("text"), caps.name("url")) {
                links.push(DocLink {
                    kind: LinkKind::Path,
                    target: url.as_str().trim().to_owned(),
                });
                return text.as_str().trim().to_owned();
            }
            let role = caps.name("role").map_or("", |m| m.as_str());
            let role = role.rsplit(':').next().unwrap_or(role).to_ascii_lowercase();
            let (title, target) = title_and_target(caps.name("body").map_or("", |m| m.as_str()));
            let target = target.trim_start_matches(['~', '!']);
            match role.as_str() {
                "doc" => {
                    links.push(DocLink {
                        kind: LinkKind::Doc,
                        target: target.to_owned(),
                    });
                    title.unwrap_or(target).to_owned()
                }
                "ref" | "term" | "numref" | "abbr" | "sub" | "sup" | "math" | "kbd"
                | "guilabel" | "menuselection" | "emphasis" | "strong" | "manpage" => {
                    title.unwrap_or(target).to_owned()
                }
                _ => format!("`{}`", target.trim_end_matches("()")),
            }
        })
        .into_owned()
}

fn asciidoc_heading(line: &str) -> Option<(u8, &str)> {
    let level = line.bytes().take_while(|b| *b == b'=').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let title = line[level..].strip_prefix(' ')?.trim();
    (!title.is_empty()).then_some((level as u8, title))
}

fn underlined_document(text: &str, flavour: Flavour) -> ParsedText {
    let lines: Vec<&str> = text.lines().collect();
    let mut builder = SectionBuilder::default();
    let mut links = Vec::new();
    // Adornment styles in order of first appearance; the position is the heading level.
    let mut styles: Vec<(char, bool)> = Vec::new();
    let mut level_of = |style: (char, bool)| -> u8 {
        let position = styles.iter().position(|known| *known == style);
        let index = position.unwrap_or_else(|| {
            styles.push(style);
            styles.len() - 1
        });
        (index + 1).min(usize::from(u8::MAX)) as u8
    };
    let is_title = |line: &str| !line.trim().is_empty() && adornment(line).is_none();
    let rst = flavour == Flavour::Rst;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if flavour == Flavour::AsciiDoc {
            match asciidoc_heading(line) {
                Some((level, title)) => builder.heading(level, title),
                None => builder.line(line),
            }
            i += 1;
            continue;
        }
        if let Some(mark) = adornment(line) {
            // Overline + title + underline.
            if i + 2 < lines.len()
                && is_title(lines[i + 1])
                && adornment(lines[i + 2]) == Some(mark)
            {
                let title = lines[i + 1].trim();
                let title = if rst {
                    rst_inline(title, &mut links)
                } else {
                    title.to_owned()
                };
                builder.heading(level_of((mark, true)), &title);
                i += 3;
            } else {
                // A transition line or table border: no content.
                i += 1;
            }
            continue;
        }
        // Title + underline. A heading starts a block, which keeps table rows and wrapped
        // paragraphs followed by a rule from being promoted.
        if is_title(line)
            && !line.starts_with([' ', '\t'])
            && (i == 0 || lines[i - 1].trim().is_empty() || adornment(lines[i - 1]).is_some())
            && i + 1 < lines.len()
            && !(rst && line.starts_with(".. "))
            && let Some(mark) = adornment(lines[i + 1])
        {
            let title_len = line.trim().chars().count();
            let rule_len = lines[i + 1].trim_end().len();
            if title_len <= 200 && rule_len * 2 >= title_len {
                let title = if rst {
                    rst_inline(line.trim(), &mut links)
                } else {
                    line.trim().to_owned()
                };
                builder.heading(level_of((mark, false)), &title);
                i += 2;
                continue;
            }
        }
        if rst {
            let trimmed = line.trim_start();
            if trimmed == ".." || trimmed.starts_with(".. ") {
                i = rst_directive(&lines, i, &mut builder, &mut links);
                continue;
            }
            if line.contains('`') {
                builder.line(&rst_inline(line, &mut links));
            } else {
                // `Example::` introduces a literal block; the marker itself is not prose.
                builder.line(
                    line.strip_suffix("::")
                        .map_or(line, |rest| if rest.trim().is_empty() { "" } else { line }),
                );
            }
        } else {
            builder.line(line);
        }
        i += 1;
    }
    ParsedText {
        sections: builder.finish(),
        links,
    }
}

/// Handles the explicit markup block starting at `lines[start]`; returns the index of the next
/// line to process.
fn rst_directive(
    lines: &[&str],
    start: usize,
    builder: &mut SectionBuilder,
    links: &mut Vec<DocLink>,
) -> usize {
    let end = block_end(lines, start);
    let rest = lines[start].trim_start().trim_start_matches("..").trim();
    let directive = rest.split_once("::").filter(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(['_', '[', '|'])
            && !name.contains(char::is_whitespace)
    });
    let Some((name, argument)) = directive else {
        // Comment, hyperlink target, footnote or substitution definition.
        return end;
    };
    let name = name.to_ascii_lowercase();
    let argument = argument.trim();
    if name == "toctree" {
        for entry in &lines[start + 1..end] {
            let entry = entry.trim();
            if entry.is_empty() || entry.starts_with(':') {
                continue;
            }
            let (_, target) = title_and_target(entry);
            links.push(DocLink {
                kind: LinkKind::Doc,
                target: target.to_owned(),
            });
        }
        return end;
    }
    if RST_ARGUMENT_IS_REFERENCE.contains(&name.as_str()) && !argument.is_empty() {
        builder.line(&format!("`{argument}`"));
    }
    if RST_DROPPED.contains(&name.as_str()) {
        return end;
    }
    if !argument.is_empty() && !RST_ARGUMENT_IS_NOISE.contains(&name.as_str()) {
        if name.contains(':') || name.contains("function") {
            // Domain directives (`c:function::`, `py:class::`) declare a symbol.
            builder.line(&format!("`{argument}`"));
        } else {
            builder.line(&rst_inline(argument, links));
        }
    }
    // Keep the body, drop the option lines (`:name: value`) that directly follow the marker.
    let mut next = start + 1;
    while next < end {
        let option = lines[next].trim();
        if option.starts_with(':') && option[1..].contains(':') {
            next += 1;
        } else {
            break;
        }
    }
    next
}

/// Splits `text` into pieces of at most `max_chars` bytes, preferring paragraph boundaries (blank
/// lines), then line boundaries, and cutting inside a line only when a single line is longer than
/// the limit. Pieces are trimmed and never empty.
pub fn split_text(text: &str, max_chars: usize) -> Vec<String> {
    let max = max_chars.max(64);
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut push_unit = |unit: &str, separator: &str, pieces: &mut Vec<String>| {
        if !current.is_empty() && current.len() + separator.len() + unit.len() > max {
            pieces.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str(separator);
        }
        current.push_str(unit);
        if current.len() > max {
            // Only reachable when `unit` alone exceeds the limit and has no finer boundary.
            let mut rest = current.as_str();
            while rest.len() > max {
                let mut cut = max;
                while !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                pieces.push(rest[..cut].trim().to_owned());
                rest = &rest[cut..];
            }
            current = rest.trim().to_owned();
        }
    };
    for paragraph in text.split("\n\n") {
        let paragraph = paragraph.trim_matches('\n').trim_end();
        if paragraph.trim().is_empty() {
            continue;
        }
        if paragraph.len() <= max {
            push_unit(paragraph, "\n\n", &mut pieces);
        } else {
            let mut first = true;
            for line in paragraph.lines() {
                push_unit(line, if first { "\n\n" } else { "\n" }, &mut pieces);
                first = false;
            }
        }
    }
    if !current.trim().is_empty() {
        pieces.push(current);
    }
    pieces.retain(|piece| !piece.trim().is_empty());
    pieces
}

/// Turns sections into chunk texts of at most about `max_chars` bytes. Every chunk starts with its
/// heading path (`Guide > Install`) on a line of its own, so a chunk quoted out of context still
/// says where it comes from. Sections without body text produce no chunk.
pub fn chunk_sections(sections: &[Section], max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    for section in sections {
        if section.body.trim().is_empty() {
            continue;
        }
        let prefix = section.heading_path.join(" > ");
        let budget = max_chars.saturating_sub(prefix.len() + 2);
        for piece in split_text(&section.body, budget) {
            if prefix.is_empty() {
                chunks.push(piece);
            } else {
                chunks.push(format!("{prefix}\n\n{piece}"));
            }
        }
    }
    chunks
}

/// Text and links extracted from an HTML element.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HtmlText {
    /// Markdown-flavoured text: headings as `#` lines, list items as `- `, table rows as cells
    /// joined with ` | `, inline code in backticks, preformatted blocks in code fences.
    pub text: String,
    /// `href`s of the anchors encountered, as written.
    pub links: Vec<DocLink>,
}

/// Converts an HTML document or fragment to text (see [`HtmlText::text`] for the shape). Scripts,
/// styles, navigation, forms and footers are dropped. Never fails: broken markup is repaired the
/// way a browser would.
pub fn html_to_text(html: &str) -> String {
    let document = Html::parse_document(html);
    element_text(document.root_element(), &|_| false).text
}

/// Converts the subtree of `root` to text, skipping every element for which `skip` returns `true`
/// (in addition to scripts, styles and navigation).
pub fn element_text(root: ElementRef<'_>, skip: &dyn Fn(&ElementRef<'_>) -> bool) -> HtmlText {
    let mut writer = HtmlWriter {
        out: String::new(),
        links: Vec::new(),
        skip,
    };
    writer.element(root, 0, false);
    HtmlText {
        text: tidy(&writer.out),
        links: writer.links,
    }
}

fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank_run > 0 { "\n\n" } else { "\n" });
        }
        blank_run = 0;
        out.push_str(line);
    }
    out
}

fn is_noise(element: &ElementRef<'_>) -> bool {
    let value = element.value();
    if matches!(
        value.name(),
        "script"
            | "style"
            | "noscript"
            | "template"
            | "nav"
            | "svg"
            | "iframe"
            | "head"
            | "footer"
            | "button"
            | "form"
            | "select"
            | "textarea"
    ) {
        return true;
    }
    if value.attr("role") == Some("navigation") {
        return true;
    }
    let id = value.id().unwrap_or("");
    matches!(id, "footer" | "navigation" | "sidebar" | "breadcrumbs")
        || id.contains("breadcrumb")
        || value.classes().any(|class| class.contains("breadcrumb"))
}

struct HtmlWriter<'s> {
    out: String,
    links: Vec<DocLink>,
    skip: &'s dyn Fn(&ElementRef<'_>) -> bool,
}

impl HtmlWriter<'_> {
    fn space(&mut self) {
        if self
            .out
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace())
        {
            self.out.push(' ');
        }
    }

    fn newline(&mut self) {
        let kept = self.out.trim_end_matches(' ').len();
        self.out.truncate(kept);
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn block(&mut self) {
        self.newline();
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn text(&mut self, text: &str, pre: bool) {
        if pre {
            self.out.push_str(text);
            return;
        }
        let mut words = text.split_whitespace();
        let Some(first) = words.next() else {
            if !text.is_empty() {
                self.space();
            }
            return;
        };
        if text.starts_with(char::is_whitespace) {
            self.space();
        }
        self.out.push_str(first);
        for word in words {
            self.out.push(' ');
            self.out.push_str(word);
        }
        if text.ends_with(char::is_whitespace) {
            self.out.push(' ');
        }
    }

    fn children(&mut self, element: ElementRef<'_>, depth: usize, pre: bool) {
        for child in element.children() {
            match child.value() {
                Node::Text(text) => self.text(text, pre),
                Node::Element(_) => {
                    if let Some(child) = ElementRef::wrap(child) {
                        self.element(child, depth + 1, pre);
                    }
                }
                _ => {}
            }
        }
    }

    /// Writes the children on a single line (headings, table cells, inline code).
    fn inline_children(&mut self, element: ElementRef<'_>, depth: usize) {
        let start = self.out.len();
        self.children(element, depth, false);
        let inner = self.out.split_off(start);
        let mut words = inner.split_whitespace();
        if let Some(first) = words.next() {
            self.out.push_str(first);
            for word in words {
                self.out.push(' ');
                self.out.push_str(word);
            }
        }
    }

    fn element(&mut self, element: ElementRef<'_>, depth: usize, pre: bool) {
        if is_noise(&element) || (self.skip)(&element) {
            return;
        }
        if depth > MAX_HTML_DEPTH {
            for text in element.text() {
                self.text(text, pre);
            }
            return;
        }
        let name = element.value().name();
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = usize::from(name.as_bytes()[1] - b'0');
                self.block();
                self.out.push_str(&"#".repeat(level));
                self.out.push(' ');
                self.inline_children(element, depth);
                self.block();
            }
            "br" => self.out.push('\n'),
            "p" | "div" | "section" | "article" | "blockquote" | "ul" | "ol" | "dl" | "table"
            | "main" | "body" | "html" | "figure" | "details" | "summary" | "dt" | "dd" | "hr"
            | "header" | "aside" | "caption" => {
                self.block();
                self.children(element, depth, pre);
                self.block();
            }
            "li" => {
                self.newline();
                self.out.push_str("- ");
                self.children(element, depth, pre);
                self.newline();
            }
            "tr" => {
                self.newline();
                self.children(element, depth, pre);
                let kept = self.out.trim_end_matches([' ', '|']).len();
                self.out.truncate(kept);
                self.newline();
            }
            "td" | "th" => {
                self.inline_children(element, depth);
                self.out.push_str(" | ");
            }
            "pre" => {
                self.block();
                self.out.push_str("```\n");
                self.children(element, depth, true);
                if !self.out.ends_with('\n') {
                    self.out.push('\n');
                }
                self.out.push_str("```");
                self.block();
            }
            "code" | "tt" | "kbd" | "samp" if !pre => {
                self.out.push('`');
                self.inline_children(element, depth);
                self.out.push('`');
            }
            "a" => {
                if let Some(href) = element.value().attr("href") {
                    self.links.push(DocLink {
                        kind: LinkKind::Path,
                        target: href.to_owned(),
                    });
                }
                self.children(element, depth, pre);
            }
            "img" => {
                if let Some(alt) = element.value().attr("alt") {
                    self.text(alt, pre);
                }
            }
            _ => self.children(element, depth, pre),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_is_split_only_when_closed() {
        let (yaml, body) = split_front_matter("---\ntitle: A\n---\n# A\n");
        assert_eq!(yaml, Some("title: A\n"));
        assert_eq!(body, "# A\n");
        assert_eq!(split_front_matter("---\ntitle: A\n").0, None);
        assert_eq!(split_front_matter("# A\n---\n").0, None);
    }

    #[test]
    fn markdown_sections_track_heading_paths_and_fences() {
        let text = "intro\n\n# Guide\n\ntop\n\n## Install\n\n```sh\n# not a heading\n```\n\nSetext\n------\n\nbody `Ledger::charge`\n\n# Other ##\n\nlast\n";
        let sections = markdown_sections(text);
        let paths: Vec<String> = sections.iter().map(|s| s.heading_path.join("/")).collect();
        assert_eq!(
            paths,
            ["", "Guide", "Guide/Install", "Guide/Setext", "Other"]
        );
        assert_eq!(sections[0].level, 0);
        assert!(sections[2].body.contains("# not a heading"));
        assert_eq!(sections[3].body, "body `Ledger::charge`");
        assert_eq!(sections[4].level, 1);
    }

    #[test]
    fn markdown_links_cover_inline_and_wiki_forms() {
        let links = markdown_links(
            "See [a](../a.md#x \"t\"), ![img](x.png), [[Page One|label]] and [[B]].",
        );
        assert_eq!(
            links,
            vec![
                DocLink {
                    kind: LinkKind::Path,
                    target: "../a.md#x".into()
                },
                DocLink {
                    kind: LinkKind::WikiTitle,
                    target: "Page One".into()
                },
                DocLink {
                    kind: LinkKind::WikiTitle,
                    target: "B".into()
                },
            ]
        );
    }

    #[test]
    fn relative_links_resolve_against_the_linking_file() {
        assert_eq!(
            resolve_relative("docs/guide/a.md", "../b.md#frag").as_deref(),
            Some("docs/b.md")
        );
        assert_eq!(
            resolve_relative("a.md", "/x/./y.md").as_deref(),
            Some("x/y.md")
        );
        assert_eq!(resolve_relative("a.md", "https://example.com/x"), None);
        assert_eq!(resolve_relative("a.md", "mailto:a@example.com"), None);
        assert_eq!(resolve_relative("a.md", "#top"), None);
        assert_eq!(strip_extension("docs/v1.2/guide.md"), "docs/v1.2/guide");
        assert_eq!(strip_extension("docs/.hidden"), "docs/.hidden");
    }

    #[test]
    fn rst_headings_roles_and_directives() {
        let text = "\
=====
Title
=====

Intro with :c:func:`kmalloc()` and ``struct page`` and :doc:`../mm/slab`.
See :ref:`Coding style <codingstyle>` and `the site <https://example.com>`_.

.. _label:

Section
-------

.. toctree::
   :maxdepth: 2

   intro
   Guide <guide/index>

.. note::
   Keep :c:type:`gfp_t` in mind.

.. image:: fig.png
   :width: 10

.. kernel-doc:: mm/slab.c
   :export:

.. c:function:: void kfree(const void *p)

Sub
~~~

Text::

    literal
";
        let parsed = rst_document(text);
        let paths: Vec<String> = parsed
            .sections
            .iter()
            .map(|s| s.heading_path.join("/"))
            .collect();
        assert_eq!(paths, ["Title", "Title/Section", "Title/Section/Sub"]);
        let intro = &parsed.sections[0].body;
        assert!(intro.contains("`kmalloc`"), "{intro}");
        assert!(intro.contains("`struct page`"));
        assert!(intro.contains("See Coding style and the site."));
        let section = &parsed.sections[1].body;
        assert!(section.contains("Keep `gfp_t` in mind."), "{section}");
        assert!(section.contains("`mm/slab.c`"));
        assert!(section.contains("`void kfree(const void *p)`"));
        for noise in [
            "toctree", "maxdepth", "fig.png", ":export:", "_label", ".. ",
        ] {
            assert!(
                !section.contains(noise) && !intro.contains(noise),
                "{noise}"
            );
        }
        assert!(parsed.sections[2].body.contains("literal"));
        let docs: Vec<&str> = parsed
            .links
            .iter()
            .filter(|l| l.kind == LinkKind::Doc)
            .map(|l| l.target.as_str())
            .collect();
        assert_eq!(docs, ["../mm/slab", "intro", "guide/index"]);
        assert!(
            parsed
                .links
                .iter()
                .any(|l| l.kind == LinkKind::Path && l.target == "https://example.com")
        );
    }

    #[test]
    fn plain_and_asciidoc_headings() {
        let plain = plain_document("Overview\n========\n\nbody\n\nnot a heading\nwrapped\n----\n");
        assert_eq!(plain.sections.len(), 1);
        assert_eq!(plain.sections[0].heading_path, ["Overview"]);
        let adoc = asciidoc_document("= Doc\n\nintro\n\n== Part\n\nbody\n");
        assert_eq!(adoc.sections[1].heading_path, ["Doc", "Part"]);
    }

    #[test]
    fn chunks_respect_the_limit_and_carry_heading_paths() {
        let paragraph = "word ".repeat(60);
        let body = format!(
            "{paragraph}\n\n{paragraph}\n\n{paragraph}\n\n{}",
            "x".repeat(900)
        );
        let sections = vec![Section {
            heading_path: vec!["Guide".into(), "Install".into()],
            level: 2,
            body,
        }];
        let chunks = chunk_sections(&sections, 400);
        assert!(chunks.len() >= 5, "{}", chunks.len());
        for chunk in &chunks {
            assert!(chunk.starts_with("Guide > Install\n\n"));
            assert!(chunk.len() <= 400, "{}", chunk.len());
        }
        // Multi-byte text is cut on character boundaries.
        let unicode = split_text(&"é".repeat(500), 100);
        assert!(unicode.iter().all(|piece| piece.len() <= 100));
        assert_eq!(unicode.concat().chars().count(), 500);
    }

    #[test]
    fn html_becomes_markdown_flavoured_text() {
        let html = "<html><head><title>T</title><style>p{}</style></head><body>\
            <nav><a href='/home'>Home</a></nav>\
            <h1>Run <em>book</em></h1><p>Call <code>Ledger::charge</code> via \
            <a href='api.html'>the API</a>.</p><script>alert(1)</script>\
            <table><tr><th>Name</th><th>Owner</th></tr><tr><td>ledger</td><td>Zoë</td></tr></table>\
            <ul><li>one</li><li>two</li></ul><pre>let x = 1;\nlet y = 2;</pre><p>unclosed";
        let document = Html::parse_document(html);
        let result = element_text(document.root_element(), &|_| false);
        assert_eq!(
            result.text,
            "# Run book\n\nCall `Ledger::charge` via the API.\n\nName | Owner\nledger | Zoë\n\n\
             - one\n- two\n\n```\nlet x = 1;\nlet y = 2;\n```\n\nunclosed"
        );
        assert_eq!(result.links.len(), 1);
        assert_eq!(result.links[0].target, "api.html");
        assert_eq!(html_to_text("<p>a<br>b</p>"), "a\nb");
    }

    #[test]
    fn deeply_nested_html_does_not_overflow() {
        let html = format!("{}deep{}", "<div>".repeat(5000), "</div>".repeat(5000));
        assert_eq!(html_to_text(&html), "deep");
    }
}
