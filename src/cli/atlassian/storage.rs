//! Confluence storage format (XHTML with `ac:` / `ri:` macros) to plain HTML.
//!
//! The storage format is already HTML, so ordinary elements pass through. Confluence-specific
//! elements degrade to readable text: user mentions become `@account`, page links their title,
//! code macros `<pre><code>`, status lozenges `[STATUS: text]`, panels block quotes, Jira macros
//! the issue key. Macros without a useful text form (table of contents, children lists, ...) are
//! dropped.

use scraper::{ElementRef, Html};

/// Tags copied through with their (tag-specific) attributes.
const PASS_THROUGH: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
    "a",
    "strong",
    "b",
    "em",
    "i",
    "u",
    "code",
    "pre",
    "br",
    "hr",
    "blockquote",
    "span",
    "div",
    "sub",
    "sup",
    "del",
    "s",
    "caption",
];
/// Elements that never have children.
const VOID: &[&str] = &["br", "hr", "img", "col", "input", "meta", "link", "wbr"];

/// Converts a storage-format body into an HTML fragment.
pub fn storage_to_html(storage: &str) -> String {
    let prepared = expand_self_closing(&replace_cdata(storage));
    let fragment = Html::parse_fragment(&prepared);
    let mut out = String::new();
    render_children(fragment.root_element(), &mut out);
    out
}

/// Converts a storage-format body into plain text, one block per line.
pub fn storage_to_text(storage: &str) -> String {
    let html = storage_to_html(storage);
    let mut spaced = String::with_capacity(html.len());
    let mut rest = html.as_str();
    while let Some(start) = rest.find('<') {
        spaced.push_str(&rest[..start]);
        let tag_end = rest[start..]
            .find('>')
            .map_or(rest.len(), |end| start + end + 1);
        let tag = &rest[start..tag_end];
        let name = tag
            .trim_start_matches(['<', '/'])
            .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .next()
            .unwrap_or("");
        if matches!(
            name,
            "p" | "br"
                | "li"
                | "tr"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "blockquote"
                | "pre"
        ) {
            spaced.push('\n');
        }
        rest = &rest[tag_end..];
    }
    spaced.push_str(rest);
    let fragment = Html::parse_fragment(&spaced);
    let text: String = fragment.root_element().text().collect();
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// HTML-escapes text (also safe inside double-quoted attributes).
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// `<![CDATA[...]]>` is not HTML: replace it by its escaped text.
fn replace_cdata(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("<![CDATA[") {
        out.push_str(&rest[..start]);
        let body = &rest[start + 9..];
        match body.find("]]>") {
            Some(end) => {
                out.push_str(&escape(&body[..end]));
                rest = &body[end + 3..];
            }
            None => {
                out.push_str(&escape(body));
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// HTML parsers ignore the `/` of `<ri:user .../>` on unknown elements, which would nest the
/// following content inside it: write such tags as `<tag ...></tag>`.
fn expand_self_closing(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len() + 16);
    let mut position = 0;
    while let Some(offset) = input[position..].find('<') {
        let start = position + offset;
        out.push_str(&input[position..start]);
        let is_tag = bytes.get(start + 1).is_some_and(u8::is_ascii_alphabetic);
        if !is_tag {
            out.push('<');
            position = start + 1;
            continue;
        }
        let mut end = start + 1;
        let mut quote = None;
        while let Some(&byte) = bytes.get(end) {
            match quote {
                Some(open) if byte == open => quote = None,
                Some(_) => {}
                None if byte == b'"' || byte == b'\'' => quote = Some(byte),
                None if byte == b'>' => break,
                None => {}
            }
            end += 1;
        }
        if end >= bytes.len() {
            out.push_str(&input[start..]);
            return out;
        }
        let tag = &input[start..=end];
        let name = tag[1..]
            .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .next()
            .unwrap_or("");
        if let Some(open) = tag.strip_suffix("/>")
            && !VOID.contains(&name.to_ascii_lowercase().as_str())
        {
            out.push_str(open.trim_end());
            out.push('>');
            out.push_str("</");
            out.push_str(name);
            out.push('>');
        } else {
            out.push_str(tag);
        }
        position = end + 1;
    }
    out.push_str(&input[position..]);
    out
}

fn render_children(element: ElementRef<'_>, out: &mut String) {
    for child in element.children() {
        if let Some(text) = child.value().as_text() {
            out.push_str(&escape(text));
        } else if let Some(element) = ElementRef::wrap(child) {
            render_element(element, out);
        }
    }
}

fn child<'a>(element: ElementRef<'a>, name: &str) -> Option<ElementRef<'a>> {
    element
        .children()
        .filter_map(ElementRef::wrap)
        .find(|child| child.value().name() == name)
}

fn text_of(element: ElementRef<'_>) -> String {
    element.text().collect::<String>().trim().to_owned()
}

/// Value of the `<ac:parameter ac:name="name">` child.
fn parameter(element: ElementRef<'_>, name: &str) -> Option<String> {
    element
        .children()
        .filter_map(ElementRef::wrap)
        .find(|child| {
            child.value().name() == "ac:parameter" && child.value().attr("ac:name") == Some(name)
        })
        .map(text_of)
        .filter(|value| !value.is_empty())
}

fn render_element(element: ElementRef<'_>, out: &mut String) {
    let name = element.value().name().to_ascii_lowercase();
    match name.as_str() {
        "script" | "style" | "ac:parameter" | "ac:plain-text-body" | "ri:user" | "ri:page"
        | "ri:attachment" | "ri:space" | "ri:url" | "ri:blog-post" => {}
        "ac:structured-macro" | "ac:macro" => render_macro(element, out),
        "ac:link" => render_link(element, out),
        "ac:image" => {
            let file = child(element, "ri:attachment")
                .and_then(|attachment| attachment.value().attr("ri:filename"))
                .or_else(|| child(element, "ri:url").and_then(|url| url.value().attr("ri:value")));
            out.push_str("<span>[image");
            if let Some(file) = file {
                out.push_str(": ");
                out.push_str(&escape(file));
            }
            out.push_str("]</span>");
        }
        "ac:emoticon" => {
            let fallback = element
                .value()
                .attr("ac:emoji-fallback")
                .or_else(|| element.value().attr("ac:name"))
                .unwrap_or("");
            out.push_str(&escape(fallback));
        }
        "ac:task-list" => {
            out.push_str("<ul>");
            render_children(element, out);
            out.push_str("</ul>");
        }
        "ac:task" => {
            let done = child(element, "ac:task-status").map(text_of).as_deref() == Some("complete");
            out.push_str(if done { "<li>[x] " } else { "<li>[ ] " });
            if let Some(body) = child(element, "ac:task-body") {
                render_children(body, out);
            }
            out.push_str("</li>");
        }
        "ac:task-id" | "ac:task-status" => {}
        "time" => {
            out.push_str(&escape(element.value().attr("datetime").unwrap_or("")));
        }
        "a" => {
            out.push_str("<a");
            if let Some(href) = element.value().attr("href") {
                out.push_str(&format!(" href=\"{}\"", escape(href)));
            }
            out.push('>');
            render_children(element, out);
            out.push_str("</a>");
        }
        tag if PASS_THROUGH.contains(&tag) => {
            out.push('<');
            out.push_str(tag);
            if matches!(tag, "td" | "th") {
                for attribute in ["colspan", "rowspan"] {
                    if let Some(value) = element.value().attr(attribute) {
                        out.push_str(&format!(" {attribute}=\"{}\"", escape(value)));
                    }
                }
            }
            out.push('>');
            if !VOID.contains(&tag) {
                render_children(element, out);
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
        // Includes `ac:rich-text-body`, `ac:inline-comment-marker`, `ac:layout*`, unknown tags:
        // keep their content.
        _ => render_children(element, out),
    }
}

fn render_link(element: ElementRef<'_>, out: &mut String) {
    let label = child(element, "ac:plain-text-link-body")
        .or_else(|| child(element, "ac:link-body"))
        .map(text_of)
        .filter(|label| !label.is_empty());
    let text = if let Some(user) = child(element, "ri:user") {
        let id = user
            .value()
            .attr("ri:account-id")
            .or_else(|| user.value().attr("ri:userkey"))
            .or_else(|| user.value().attr("ri:username"))
            .unwrap_or("unknown");
        format!("@{id}")
    } else if let Some(page) = child(element, "ri:page") {
        label
            .or_else(|| page.value().attr("ri:content-title").map(str::to_owned))
            .unwrap_or_default()
    } else if let Some(attachment) = child(element, "ri:attachment") {
        label
            .or_else(|| attachment.value().attr("ri:filename").map(str::to_owned))
            .unwrap_or_default()
    } else if let Some(space) = child(element, "ri:space") {
        label
            .or_else(|| space.value().attr("ri:space-key").map(str::to_owned))
            .unwrap_or_default()
    } else {
        label.unwrap_or_default()
    };
    out.push_str("<span>");
    out.push_str(&escape(&text));
    out.push_str("</span>");
}

fn render_macro(element: ElementRef<'_>, out: &mut String) {
    let name = element.value().attr("ac:name").unwrap_or("");
    match name {
        "code" | "noformat" => {
            let body = child(element, "ac:plain-text-body")
                .map(|body| body.text().collect::<String>())
                .unwrap_or_default();
            match parameter(element, "language") {
                Some(language) => out.push_str(&format!(
                    "<pre><code class=\"language-{}\">",
                    escape(&language)
                )),
                None => out.push_str("<pre><code>"),
            }
            out.push_str(&escape(&body));
            out.push_str("</code></pre>");
        }
        "status" => {
            let label = parameter(element, "title")
                .or_else(|| parameter(element, "colour"))
                .unwrap_or_default();
            out.push_str(&format!("<span>[STATUS: {}]</span>", escape(&label)));
        }
        "jira" => {
            if let Some(key) = parameter(element, "key") {
                out.push_str(&format!("<span>{}</span>", escape(&key)));
            }
        }
        "info" | "note" | "warning" | "tip" | "panel" => {
            let label = match name {
                "info" => "Info",
                "note" => "Note",
                "warning" => "Warning",
                "tip" => "Tip",
                _ => "Panel",
            };
            out.push_str(&format!("<blockquote><strong>{label}:</strong> "));
            if let Some(title) = parameter(element, "title") {
                out.push_str(&format!("<strong>{}</strong> ", escape(&title)));
            }
            if let Some(body) = child(element, "ac:rich-text-body") {
                render_children(body, out);
            }
            out.push_str("</blockquote>");
        }
        "expand" => {
            if let Some(title) = parameter(element, "title") {
                out.push_str(&format!("<p><strong>{}</strong></p>", escape(&title)));
            }
            if let Some(body) = child(element, "ac:rich-text-body") {
                render_children(body, out);
            }
        }
        // Macros that only decorate or list other pages carry no text of their own.
        "toc" | "children" | "recently-updated" | "pagetree" | "anchor" | "excerpt-include"
        | "include" | "contentbylabel" | "livesearch" | "profile-picture" => {}
        _ => {
            if let Some(body) = child(element, "ac:rich-text-body") {
                render_children(body, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macros_degrade_to_readable_text() {
        let storage = concat!(
            "<p>Ping <ac:link><ri:user ri:account-id=\"abc123\"/></ac:link> about ",
            "<ac:link><ri:page ri:content-title=\"Ledger Design\"/></ac:link> ",
            "<ac:structured-macro ac:name=\"status\"><ac:parameter ac:name=\"title\">BLOCKED</ac:parameter></ac:structured-macro>.</p>",
            "<ac:structured-macro ac:name=\"code\"><ac:parameter ac:name=\"language\">rust</ac:parameter>",
            "<ac:plain-text-body><![CDATA[fn main() { if a < b {} }]]></ac:plain-text-body></ac:structured-macro>",
            "<ac:structured-macro ac:name=\"info\"><ac:rich-text-body><p>Heads up</p></ac:rich-text-body></ac:structured-macro>",
            "<ac:structured-macro ac:name=\"toc\"/><p>after</p>",
        );
        let html = storage_to_html(storage);
        assert!(html.contains("@abc123"), "{html}");
        assert!(html.contains("Ledger Design"), "{html}");
        assert!(html.contains("[STATUS: BLOCKED]"), "{html}");
        assert!(
            html.contains("<code class=\"language-rust\">fn main() { if a &lt; b {} }</code>"),
            "{html}"
        );
        assert!(
            html.contains("<blockquote><strong>Info:</strong> <p>Heads up</p>"),
            "{html}"
        );
        assert!(
            html.contains("<p>after</p>"),
            "self-closing macro must not swallow: {html}"
        );
        assert!(!html.contains("ac:"), "{html}");
    }

    #[test]
    fn text_conversion_keeps_blocks_apart() {
        let text = storage_to_text("<p>One <strong>two</strong></p><p>Three</p>");
        assert_eq!(text, "One two\nThree");
    }
}
