//! Jira rich text to plain text: Atlassian Document Format and wiki markup.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// Nesting beyond this is ignored; real documents are a handful of levels deep.
const MAX_DEPTH: usize = 64;

static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{\{(.+?)\}\}").expect("constant pattern, covered by tests, compiles")
});
static CODE_FENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{(?:code|noformat)(?::[^}\n]*)?\}")
        .expect("constant pattern, covered by tests, compiles")
});

/// Converts Jira wiki markup to text, keeping code recognisable: `{{inline}}` becomes a backticked
/// span and `{code}` / `{noformat}` blocks become fenced blocks. Other markup is left as written.
pub fn markup_to_text(markup: &str) -> String {
    let text = INLINE_CODE.replace_all(markup, "`$1`");
    CODE_FENCE.replace_all(&text, "```").trim().to_owned()
}

/// Converts an Atlassian Document Format node (usually the `doc` root) to text.
///
/// Blocks are separated by blank lines, list items are prefixed with `- ` or `1. `, inline code
/// marks become backticked spans and code blocks become fenced blocks. Unknown node types
/// contribute the text of their children; media is dropped.
pub fn adf_to_text(document: &Value) -> String {
    render(document, 0).trim().to_owned()
}

/// Text of a description or comment body in either representation; `None` when blank.
pub(super) fn rich_text(value: Option<&Value>) -> Option<String> {
    let text = match value? {
        Value::String(markup) => markup_to_text(markup),
        document @ Value::Object(_) => adf_to_text(document),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

fn render(node: &Value, depth: usize) -> String {
    if depth > MAX_DEPTH {
        return String::new();
    }
    let kind = node.get("type").and_then(Value::as_str).unwrap_or("");
    let children: &[Value] = node
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let attr = |name: &str| {
        node.get("attrs")
            .and_then(|attrs| attrs.get(name))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let inline = || {
        children
            .iter()
            .map(|child| render(child, depth + 1))
            .collect::<String>()
    };
    let blocks = |separator: &str| {
        children
            .iter()
            .map(|child| render(child, depth + 1))
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join(separator)
    };
    let list = |marker: &dyn Fn(usize) -> String| {
        children
            .iter()
            .map(|child| render(child, depth + 1))
            .filter(|text| !text.trim().is_empty())
            .enumerate()
            .map(|(index, item)| format!("{}{}", marker(index), item.replace('\n', "\n  ")))
            .collect::<Vec<_>>()
            .join("\n")
    };
    match kind {
        "text" => text_node(node),
        "hardBreak" => "\n".to_owned(),
        "mention" | "status" => attr("text"),
        "emoji" => {
            let text = attr("text");
            if text.is_empty() {
                attr("shortName")
            } else {
                text
            }
        }
        "inlineCard" | "blockCard" | "embedCard" => attr("url"),
        "date" => attr("timestamp"),
        "rule" => "---".to_owned(),
        "media" | "mediaGroup" | "mediaSingle" => String::new(),
        "codeBlock" => {
            let code: String = children
                .iter()
                .filter_map(|child| child.get("text").and_then(Value::as_str))
                .collect();
            format!("```{}\n{}\n```", attr("language"), code.trim_end())
        }
        "paragraph" | "heading" | "taskItem" | "decisionItem" => inline(),
        "blockquote" => blocks("\n\n")
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        "bulletList" | "taskList" | "decisionList" => list(&|_| "- ".to_owned()),
        "orderedList" => list(&|index| format!("{}. ", index + 1)),
        "listItem" | "table" => blocks("\n"),
        "tableRow" => blocks(" | "),
        "tableCell" | "tableHeader" => blocks(" "),
        _ if children.is_empty() => node
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        _ => blocks("\n\n"),
    }
}

fn text_node(node: &Value) -> String {
    let text = node.get("text").and_then(Value::as_str).unwrap_or("");
    let marks: &[Value] = node
        .get("marks")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let mark = |name: &str| {
        marks
            .iter()
            .find(|mark| mark.get("type").and_then(Value::as_str) == Some(name))
    };
    if mark("code").is_some() {
        return format!("`{text}`");
    }
    let href = mark("link")
        .and_then(|link| link.get("attrs"))
        .and_then(|attrs| attrs.get("href"))
        .and_then(Value::as_str);
    match href {
        Some(href) if href != text => format!("{text} ({href})"),
        _ => text.to_owned(),
    }
}
