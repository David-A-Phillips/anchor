//! Intent parsing and query classification (System 1).
//!
//! Two classifiers produce a [`QueryIntent`]: the System 1 model, driven by [`intent_request`] and
//! read back through the forgiving [`parse_intent`], and the deterministic
//! [`HeuristicClassifier`], used when no model is configured or reachable and to fill whatever
//! the model left out or got wrong.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::base::{
    CallPurpose, ClassifierKind, CompletionRequest, Complexity, EntityMention, IntentKind,
    NodeKind, QueryIntent, ResponseFormat,
};
use crate::engine::ingest::mentions::extract_mentions;

/// Longest query text sent to the model; anything longer is cut (classification needs the gist).
const MAX_QUERY_CHARS: usize = 2_000;
/// Upper bound on entities in one intent.
const MAX_ENTITIES: usize = 16;

/// Kinds of node that answer a question of the given intent, most likely first.
pub fn default_target_kinds(kind: IntentKind) -> Vec<NodeKind> {
    use NodeKind::{
        ApiEndpoint, ApiSpec, Approval, Comment, Component, File, Person, PullRequest, Service,
        Symbol, Team, Thread, Ticket, WikiPage,
    };
    match kind {
        IntentKind::Ownership => vec![Team, Person],
        IntentKind::Justification => vec![Approval, Thread, Comment, Ticket, PullRequest],
        IntentKind::Dependency => vec![Service, Component, ApiSpec, ApiEndpoint, Symbol],
        IntentKind::Definition => vec![Symbol, File, ApiSpec, ApiEndpoint],
        IntentKind::Status => vec![Ticket, Approval, PullRequest, Thread],
        IntentKind::HowTo => vec![WikiPage],
        IntentKind::General => Vec::new(),
    }
}

/// Deterministic rule-based classifier. Stateless and cheap (a few regex scans).
#[derive(Clone, Copy, Debug, Default)]
pub struct HeuristicClassifier;

fn has_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn starts_any(text: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| text.starts_with(prefix))
}

/// Words describing a finished or decided state, for "is X approved?"-style questions.
const STATE_WORDS: &[&str] = &[
    "approved",
    "done",
    "merged",
    "resolved",
    "closed",
    "open",
    "finished",
    "complete",
    "completed",
    "deployed",
    "released",
    "ready",
    "rejected",
    "dismissed",
    "shipped",
    "live",
    "pending",
    "in progress",
    "signed off",
];

fn intent_kind(lower: &str) -> IntentKind {
    let padded = format!(" {lower} ");
    let q = padded.as_str();
    if lower.starts_with("why")
        || has_any(
            q,
            &[
                " why ",
                "what is blocking",
                "what's blocking",
                "what blocks",
                "whats blocking",
                "what is preventing",
                "what prevents",
                "reason for",
                "reason behind",
                "reason why",
                "reasons for",
                "how come",
                "rationale",
                "justification",
            ],
        )
    {
        return IntentKind::Justification;
    }
    if has_any(
        q,
        &[
            "who owns",
            "who own ",
            "who maintains",
            "who is responsible",
            "who's responsible",
            "responsible for",
            "on call",
            "on-call",
            "oncall",
            "contact for",
            "who do i contact",
            "who should i contact",
            "who should i ask",
            "who do i ask",
            "who do i talk",
            "who to contact",
            "owner of",
            "owners of",
            "owned by",
            "team owns",
            "team maintains",
            "maintainer",
            "point of contact",
            "who runs",
            "who manages",
            "who is the owner",
            "who looks after",
            "maintained by",
            "who is in charge",
            "ownership of",
        ],
    ) {
        return IntentKind::Ownership;
    }
    let is_question_about_state = starts_any(
        lower,
        &["is ", "are ", "has ", "have ", "was ", "were ", "did "],
    ) && has_any(q, STATE_WORDS);
    if is_question_about_state
        || has_any(
            q,
            &[
                "status of",
                " status ",
                "state of",
                "progress on",
                "progress of",
                "still open",
                "approved yet",
                "been approved",
                "been merged",
                "been resolved",
                "any update",
                "latest on",
            ],
        )
    {
        return IntentKind::Status;
    }
    if starts_any(
        lower,
        &[
            "how do i",
            "how do we",
            "how do you",
            "how to",
            "how can",
            "how should",
            "how would i",
        ],
    ) || has_any(
        q,
        &[
            " how to ",
            "steps to",
            "instructions for",
            "guide for",
            "guide to",
            "what is the process",
            "what's the process",
            "procedure for",
            "how do i",
            "runbook",
        ],
    ) {
        return IntentKind::HowTo;
    }
    if has_any(
        q,
        &[
            "depend",
            "what uses",
            "who uses",
            " use the ",
            "that use ",
            " uses ",
            "consumers of",
            "consumer of",
            "what breaks",
            "what would break",
            "what will break",
            "impact of",
            "impacted",
            "affected",
            "upstream",
            "downstream",
            "who calls",
            "what calls",
            "callers of",
            "relies on",
            "rely on",
            "used by",
            "blast radius",
            "clients of",
        ],
    ) {
        return IntentKind::Dependency;
    }
    if has_any(
        q,
        &[
            "where is",
            "where are",
            "where's",
            "where does",
            "where do ",
            "where can i find",
            " defined",
            " implemented",
            "implementation of",
            "definition of",
            " declared",
            "which file",
            "what file",
            "source of",
            "show me the code",
        ],
    ) {
        return IntentKind::Definition;
    }
    IntentKind::General
}

/// Words that follow "API", "service", ... without being a name.
const NOT_A_NAME: &[&str] = &[
    "the",
    "a",
    "an",
    "this",
    "that",
    "these",
    "those",
    "is",
    "are",
    "was",
    "were",
    "be",
    "for",
    "of",
    "to",
    "in",
    "on",
    "at",
    "by",
    "with",
    "and",
    "or",
    "not",
    "it",
    "its",
    "our",
    "my",
    "your",
    "their",
    "which",
    "what",
    "who",
    "whom",
    "why",
    "how",
    "where",
    "when",
    "owns",
    "own",
    "owner",
    "maintains",
    "does",
    "do",
    "did",
    "has",
    "have",
    "had",
    "can",
    "could",
    "should",
    "would",
    "will",
    "any",
    "all",
    "some",
    "each",
    "every",
    "new",
    "old",
    "other",
    "same",
    "internal",
    "external",
    "public",
    "private",
    "blocking",
    "blocked",
    "approved",
    "rejected",
    "depend",
    "depends",
    "use",
    "uses",
    "call",
    "calls",
    "from",
    "as",
    "if",
    "there",
    "here",
    "being",
    "been",
    "get",
    "gets",
    "still",
    "yet",
    "called",
    "named",
    "api",
    "service",
    "team",
    "ticket",
    "component",
    "domain",
    "about",
    "status",
    "approval",
    "portal",
    "definition",
    "spec",
    "review",
    "so",
    "but",
    "than",
    "then",
    "responsible",
    "currently",
    "right",
    "now",
    "we",
    "i",
    "they",
    "he",
    "she",
    "us",
    "me",
    "between",
    "into",
    "after",
    "before",
    "implemented",
    "defined",
    "done",
    "open",
    "closed",
];

static QUOTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""([^"\n]{2,80})"|`([^`\n]{2,80})`|(?:^|[\s(])'([^'\n]{2,80})'"#)
        .expect("static regex is valid")
});
static ROUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:\b(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+)?(/[A-Za-z0-9_{}.\-]+(?:/[A-Za-z0-9_{}.:\-]+)+)",
    )
    .expect("static regex is valid")
});
static AFTER_KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(api|service|team|ticket|component|domain)\s+([A-Za-z0-9][\w./:\-]*)")
        .expect("static regex is valid")
});
static BEFORE_KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([A-Za-z0-9][\w.\-]*)\s+(api|service|team|component|domain)\b")
        .expect("static regex is valid")
});

fn keyword_kind(keyword: &str) -> NodeKind {
    match keyword.to_ascii_lowercase().as_str() {
        "api" => NodeKind::ApiSpec,
        "service" => NodeKind::Service,
        "team" => NodeKind::Team,
        "ticket" => NodeKind::Ticket,
        "component" => NodeKind::Component,
        _ => NodeKind::Domain,
    }
}

fn clean_name(name: &str) -> &str {
    name.trim_end_matches(['?', '.', ',', '!', ':', ';', ')', '\'', '"'])
}

/// Entities referred to in `query`, deduplicated, in order of appearance.
fn extract_entities(query: &str) -> Vec<EntityMention> {
    // (byte offset, mention); sources are added most specific first so that deduplication keeps
    // the best kind hint.
    let mut found: Vec<(usize, EntityMention)> = Vec::new();
    let mut add = |pos: usize, text: &str, kind: Option<NodeKind>| {
        let text = text.trim();
        if text.is_empty() || found.iter().any(|(_, m)| m.text.eq_ignore_ascii_case(text)) {
            return;
        }
        found.push((
            pos,
            EntityMention {
                text: text.to_owned(),
                kind_hint: kind,
            },
        ));
    };

    for caps in ROUTE.captures_iter(query) {
        let Some(route) = caps.get(2) else { continue };
        // A path glued to a preceding word character is a file path fragment, not a route.
        let glued = query[..route.start()]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '.' || c == '_');
        if glued {
            continue;
        }
        match caps.get(1) {
            Some(method) => add(
                method.start(),
                &format!("{} {}", method.as_str(), clean_name(route.as_str())),
                Some(NodeKind::ApiEndpoint),
            ),
            None => add(
                route.start(),
                clean_name(route.as_str()),
                Some(NodeKind::ApiEndpoint),
            ),
        }
    }
    for caps in QUOTED.captures_iter(query) {
        if let Some(inner) = caps.get(1).or(caps.get(2)).or(caps.get(3)) {
            add(inner.start(), inner.as_str(), None);
        }
    }
    for caps in AFTER_KEYWORD.captures_iter(query) {
        let (Some(keyword), Some(name)) = (caps.get(1), caps.get(2)) else {
            continue;
        };
        let text = clean_name(name.as_str());
        if !NOT_A_NAME.contains(&text.to_ascii_lowercase().as_str()) {
            add(name.start(), text, Some(keyword_kind(keyword.as_str())));
        }
    }
    for caps in BEFORE_KEYWORD.captures_iter(query) {
        let (Some(name), Some(keyword)) = (caps.get(1), caps.get(2)) else {
            continue;
        };
        if !NOT_A_NAME.contains(&name.as_str().to_ascii_lowercase().as_str()) {
            add(
                name.start(),
                name.as_str(),
                Some(keyword_kind(keyword.as_str())),
            );
        }
    }
    for mention in extract_mentions(query) {
        let pos = query.find(&mention.lookup.text).unwrap_or(usize::MAX);
        add(
            pos,
            &mention.lookup.text,
            mention.lookup.kinds.first().copied(),
        );
    }

    // A mention contained in a longer one ("/v1/charges" inside "POST /v1/charges") adds nothing.
    let lowered: Vec<String> = found.iter().map(|(_, m)| m.text.to_lowercase()).collect();
    let mut kept: Vec<(usize, EntityMention)> = found
        .into_iter()
        .enumerate()
        .filter(|(index, _)| {
            !lowered.iter().enumerate().any(|(other, text)| {
                other != *index
                    && text.len() > lowered[*index].len()
                    && text.contains(&lowered[*index])
            })
        })
        .map(|(_, entry)| entry)
        .collect();
    kept.sort_by_key(|(pos, _)| *pos);
    kept.truncate(MAX_ENTITIES);
    kept.into_iter().map(|(_, mention)| mention).collect()
}

fn complexity(lower: &str, kind: IntentKind, entities: usize) -> Complexity {
    let q = format!(" {lower} ");
    let reasoning = kind == IntentKind::Justification
        || has_any(
            &q,
            &[
                " why ",
                "explain",
                "compare",
                "comparison",
                "summarise",
                "summarize",
                "summary",
                "difference between",
                "differences between",
                "trade-off",
                "tradeoff",
                " versus ",
                " vs ",
                "pros and cons",
                "what would happen",
                "should we",
                "analyse",
                "analyze",
                "overview",
            ],
        );
    if reasoning || entities >= 3 {
        Complexity::Complex
    } else if entities == 2 || matches!(kind, IntentKind::Dependency | IntentKind::General) {
        Complexity::Moderate
    } else {
        Complexity::Simple
    }
}

impl HeuristicClassifier {
    /// Classifies `query` with deterministic rules.
    ///
    /// Confidence is 0.8 when a rule matched and 0.3 for the `General` catch-all.
    pub fn classify(&self, query: &str) -> QueryIntent {
        let lower = query.trim().to_lowercase();
        let kind = intent_kind(&lower);
        let entities = extract_entities(query);
        QueryIntent {
            kind,
            target_kinds: default_target_kinds(kind),
            complexity: complexity(&lower, kind, entities.len()),
            confidence: if kind == IntentKind::General {
                0.3
            } else {
                0.8
            },
            classifier: ClassifierKind::Heuristic,
            entities,
        }
    }
}

fn enum_values<T: Copy>(all: &[T], name: impl Fn(T) -> &'static str) -> Vec<&'static str> {
    all.iter().map(|value| name(*value)).collect()
}

/// JSON Schema of the model's answer. Kept within the subset every provider can enforce: closed
/// objects, enums, no numeric or length constraints.
pub fn intent_schema() -> Value {
    let kinds = enum_values(NodeKind::ALL, NodeKind::as_str);
    let mut entity_kinds = kinds.clone();
    entity_kinds.push("unknown");
    json!({
        "type": "object",
        "properties": {
            "intent": {"type": "string", "enum": enum_values(IntentKind::ALL, IntentKind::as_str)},
            "entities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"},
                        "kind": {"type": "string", "enum": entity_kinds}
                    },
                    "required": ["text", "kind"],
                    "additionalProperties": false
                }
            },
            "target_kinds": {"type": "array", "items": {"type": "string", "enum": kinds}},
            "complexity": {
                "type": "string",
                "enum": enum_values(Complexity::ALL, Complexity::as_str)
            },
            "confidence": {"type": "number"}
        },
        "required": ["intent", "entities", "target_kinds", "complexity", "confidence"],
        "additionalProperties": false
    })
}

const INTENT_SYSTEM: &str = r#"You classify questions about a software organisation's code, APIs, teams, wiki and tickets. Reply with one JSON object and nothing else.

Fields:
- intent: ownership (who owns/maintains/is on call) | justification (why something is blocked, rejected, dismissed, failing) | dependency (what depends on / uses / breaks) | definition (where something is defined or implemented) | status (state of a ticket, approval, thread) | how_to | general
- entities: things named in the question, copied verbatim, each {"text", "kind"}; kind is one of portfolio, domain, component, service, team, person, file, symbol, api_spec, api_endpoint, approval, wiki_page, thread, comment, ticket, pull_request, unknown
- target_kinds: kinds of thing that would answer the question
- complexity: simple (one lookup) | moderate (a few hops) | complex (needs explanation, comparison or synthesis)
- confidence: 0 to 1

Examples:
Q: Who owns API payments?
{"intent":"ownership","entities":[{"text":"payments","kind":"api_spec"}],"target_kinds":["team","person"],"complexity":"simple","confidence":0.95}
Q: Why is PAY-123 blocking the release?
{"intent":"justification","entities":[{"text":"PAY-123","kind":"ticket"}],"target_kinds":["approval","thread","ticket"],"complexity":"complex","confidence":0.9}
Q: What breaks if we change POST /v1/charges?
{"intent":"dependency","entities":[{"text":"POST /v1/charges","kind":"api_endpoint"}],"target_kinds":["service","api_endpoint","symbol"],"complexity":"moderate","confidence":0.9}
Q: Where is LedgerWriter implemented?
{"intent":"definition","entities":[{"text":"LedgerWriter","kind":"symbol"}],"target_kinds":["symbol","file"],"complexity":"simple","confidence":0.9}"#;

/// Builds the classification request for the System 1 model.
pub fn intent_request(query: &str, query_id: Option<&str>) -> CompletionRequest {
    let mut text: String = query.trim().chars().take(MAX_QUERY_CHARS).collect();
    text.insert_str(0, "Q: ");
    let mut request = CompletionRequest::prompt(CallPurpose::Classify, text)
        .with_system(INTENT_SYSTEM)
        .with_max_tokens(400)
        .with_temperature(0.0)
        .with_format(ResponseFormat::JsonSchema(intent_schema()));
    request.query_id = query_id.map(str::to_owned);
    request
}

/// Finds the end of the JSON value starting at `text[0]` (`{` or `[`), respecting strings.
fn balanced_end(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + c.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

/// Closes the strings, arrays and objects a truncated JSON document left open.
fn close_truncated(text: &str) -> String {
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut repaired = text.trim_end().to_owned();
    if escaped {
        repaired.pop();
    }
    if in_string {
        repaired.push('"');
    }
    let trimmed = repaired.trim_end().trim_end_matches(',').to_owned();
    repaired = trimmed;
    if repaired.ends_with(':') {
        repaired.push_str("null");
    }
    repaired.extend(stack.into_iter().rev());
    repaired
}

/// Extracts a JSON value of the wanted shape from model output that may wrap it in prose or code
/// fences, or stop mid-document. `open` is `'{'` for an object or `'['` for an array.
pub(super) fn extract_json(text: &str, open: char) -> Option<Value> {
    let accepts = |value: &Value| match open {
        '{' => value.is_object(),
        _ => value.is_array(),
    };
    if let Ok(value) = serde_json::from_str::<Value>(text.trim())
        && accepts(&value)
    {
        return Some(value);
    }
    let starts: Vec<usize> = text.match_indices(open).map(|(index, _)| index).collect();
    for &start in &starts {
        if let Some(end) = balanced_end(&text[start..])
            && let Ok(value) = serde_json::from_str::<Value>(&text[start..start + end])
            && accepts(&value)
        {
            return Some(value);
        }
    }
    // Nothing balanced: the output was probably cut off by the token limit.
    let start = *starts.first()?;
    let tail = text[start..].trim_end().trim_end_matches('`').trim_end();
    serde_json::from_str::<Value>(&close_truncated(tail))
        .ok()
        .filter(accepts)
}

fn normalize_label(label: &str) -> String {
    label.trim().to_lowercase().replace([' ', '-'], "_")
}

fn parse_intent_kind(label: &str) -> Option<IntentKind> {
    let label = normalize_label(label);
    if let Ok(kind) = label.parse::<IntentKind>() {
        return Some(kind);
    }
    Some(match label.as_str() {
        "owner" | "owners" | "owner_lookup" | "who" => IntentKind::Ownership,
        "why" | "reason" | "blocker" | "explanation" => IntentKind::Justification,
        "dependencies" | "impact" | "usage" => IntentKind::Dependency,
        "definitions" | "implementation" | "location" | "where" => IntentKind::Definition,
        "state" | "progress" => IntentKind::Status,
        "howto" | "how" | "procedure" => IntentKind::HowTo,
        "other" | "unknown" => IntentKind::General,
        _ => return None,
    })
}

fn parse_node_kind(label: &str) -> Option<NodeKind> {
    let label = normalize_label(label);
    if let Ok(kind) = label.parse::<NodeKind>() {
        return Some(kind);
    }
    Some(match label.as_str() {
        "api" | "apispec" => NodeKind::ApiSpec,
        "endpoint" | "route" | "apiendpoint" => NodeKind::ApiEndpoint,
        "wiki" | "page" | "document" | "wikipage" => NodeKind::WikiPage,
        "pr" | "pullrequest" | "merge_request" => NodeKind::PullRequest,
        "issue" | "jira" => NodeKind::Ticket,
        "user" | "owner" => NodeKind::Person,
        "function" | "class" | "method" | "type" => NodeKind::Symbol,
        "repo" | "repository" => NodeKind::Service,
        _ => return None,
    })
}

fn first_str<'a>(object: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| object.get(key)?.as_str())
}

static INTENT_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)"?\bintent"?\s*[:=]\s*"?([A-Za-z_\- ]+)"#).expect("static regex is valid")
});

/// Interprets the System 1 model's answer to an [`intent_request`] for `query`.
///
/// Repairs what can be repaired: JSON wrapped in prose or code fences or cut off mid-document,
/// label spelling (`How-To`, `owner`), unknown enum values (dropped), entities that do not occur
/// in the query (dropped as hallucinations), and missing fields (taken from the
/// [`HeuristicClassifier`]). Returns `None` when the output carries no usable intent at all, in
/// which case the caller falls back to the heuristic.
pub fn parse_intent(output: &str, query: &str) -> Option<QueryIntent> {
    let object = extract_json(output, '{').or_else(|| {
        // Last resort for mangled output: a bare `intent: label` somewhere in the text.
        let label = INTENT_FIELD.captures(output)?.get(1)?.as_str().to_owned();
        Some(json!({"intent": label}))
    })?;
    let heuristic = HeuristicClassifier.classify(query);
    let model_kind =
        first_str(&object, &["intent", "kind", "intent_kind", "type"]).and_then(parse_intent_kind);

    let query_lower = query.to_lowercase();
    let mut entities: Vec<EntityMention> = Vec::new();
    for item in object
        .get("entities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (text, kind) = match item {
            Value::String(text) => (text.as_str(), None),
            Value::Object(_) => (
                first_str(item, &["text", "name", "mention", "entity"]).unwrap_or(""),
                first_str(item, &["kind", "kind_hint", "type"]).and_then(parse_node_kind),
            ),
            _ => continue,
        };
        let text = text.trim();
        if text.is_empty()
            || !query_lower.contains(&text.to_lowercase())
            || entities.iter().any(|e| e.text.eq_ignore_ascii_case(text))
            || entities.len() >= MAX_ENTITIES
        {
            continue;
        }
        entities.push(EntityMention {
            text: text.to_owned(),
            kind_hint: kind,
        });
    }

    // An object with neither a recognisable intent nor entities tells us nothing.
    if model_kind.is_none() && entities.is_empty() {
        return None;
    }
    let kind = model_kind.unwrap_or(heuristic.kind);
    if entities.is_empty() {
        entities = heuristic.entities;
    }

    let mut target_kinds: Vec<NodeKind> = Vec::new();
    for label in object
        .get("target_kinds")
        .or_else(|| object.get("targets"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(kind) = parse_node_kind(label)
            && !target_kinds.contains(&kind)
        {
            target_kinds.push(kind);
        }
    }
    if target_kinds.is_empty() {
        target_kinds = default_target_kinds(kind);
    }

    let complexity = first_str(&object, &["complexity"])
        .and_then(|label| normalize_label(label).parse::<Complexity>().ok())
        .unwrap_or(heuristic.complexity);
    let confidence = object
        .get("confidence")
        .and_then(|value| match value {
            Value::Number(number) => number.as_f64(),
            Value::String(text) => text.trim().parse().ok(),
            _ => None,
        })
        .filter(|value| value.is_finite())
        .map_or(0.6, |value| value.clamp(0.0, 1.0) as f32);
    // An intent the model did not actually state deserves less trust.
    let confidence = if model_kind.is_some() {
        confidence
    } else {
        confidence.min(0.5)
    };

    Some(QueryIntent {
        kind,
        entities,
        target_kinds,
        complexity,
        confidence,
        classifier: ClassifierKind::Model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(query: &str) -> QueryIntent {
        HeuristicClassifier.classify(query)
    }

    #[test]
    fn heuristic_intent_accuracy_on_labelled_set() {
        use IntentKind::*;
        let labelled: &[(&str, IntentKind)] = &[
            ("Who owns API X?", Ownership),
            ("Who owns the payments API?", Ownership),
            ("Who maintains ledger-service?", Ownership),
            ("Who is responsible for the checkout component?", Ownership),
            ("Who is on call for billing-gateway this week?", Ownership),
            (
                "Who do I contact for questions about POST /v1/charges?",
                Ownership,
            ),
            ("Which team owns the fraud-scoring service?", Ownership),
            ("What is the contact for the identity domain?", Ownership),
            ("owner of src/ledger/writer.rs", Ownership),
            (
                "Is there a maintainer listed for `LedgerWriter`?",
                Ownership,
            ),
            ("Why is this blocking?", Justification),
            ("Why is PAY-123 blocked?", Justification),
            (
                "Why was the refunds API rejected by the approval portal?",
                Justification,
            ),
            ("Why was the thread about retries dismissed?", Justification),
            ("Why is the build failing on payments-api?", Justification),
            (
                "What is blocking the release of ledger-service?",
                Justification,
            ),
            ("What's blocking PAY-77?", Justification),
            ("Reason for the rejection of the charges API", Justification),
            (
                "Can you tell me why the migration ticket is stuck?",
                Justification,
            ),
            (
                "How come the approval for GET /v2/users was dismissed?",
                Justification,
            ),
            ("What depends on ledger-service?", Dependency),
            ("What does payments-api depend on?", Dependency),
            ("Which services use the charges API?", Dependency),
            ("List the consumers of POST /v1/charges", Dependency),
            ("What breaks if we remove `charge_card`?", Dependency),
            (
                "What is the impact of changing the refunds API?",
                Dependency,
            ),
            ("Who calls LedgerWriter::append?", Dependency),
            ("Show downstream services of billing-gateway", Dependency),
            ("Where is LedgerWriter defined?", Definition),
            ("Where is the retry policy implemented?", Definition),
            ("Where's the handler for POST /v1/charges?", Definition),
            ("Which file contains charge_card?", Definition),
            ("Show me the definition of PaymentIntent", Definition),
            ("What is the status of PAY-123?", Status),
            ("Is the refunds API approved?", Status),
            ("Is PAY-9 done?", Status),
            (
                "Has the pull request for ledger-service been merged?",
                Status,
            ),
            ("Any update on the approval of the charges API?", Status),
            ("Status of the migration ticket", Status),
            ("How do I rotate the signing keys?", HowTo),
            ("How to request API approval?", HowTo),
            ("How can I run ledger-service locally?", HowTo),
            ("What are the steps to onboard a new service?", HowTo),
            ("What is the process for deprecating an endpoint?", HowTo),
            ("Tell me about the payments domain", General),
            ("payments-api", General),
            ("List all open threads", General),
        ];
        assert!(labelled.len() >= 40);
        let wrong: Vec<_> = labelled
            .iter()
            .filter(|(query, expected)| classify(query).kind != *expected)
            .map(|(query, expected)| (query, expected, classify(query).kind))
            .collect();
        let accuracy = 1.0 - wrong.len() as f64 / labelled.len() as f64;
        assert!(
            accuracy >= 0.9,
            "accuracy {accuracy:.2}, misclassified: {wrong:#?}"
        );
        // The two forms the product requirement names must be right, not merely likely.
        assert_eq!(classify("Who owns API X?").kind, Ownership);
        assert_eq!(classify("Why is this blocking?").kind, Justification);
    }

    #[test]
    fn heuristic_entities_targets_and_complexity() {
        let intent = classify("Who owns API X?");
        assert_eq!(
            intent.entities,
            vec![EntityMention::with_kind("X", NodeKind::ApiSpec)]
        );
        assert_eq!(intent.target_kinds, vec![NodeKind::Team, NodeKind::Person]);
        assert_eq!(intent.complexity, Complexity::Simple);
        assert_eq!(intent.classifier, ClassifierKind::Heuristic);
        assert!(intent.confidence > 0.5);

        let intent = classify("Why is this blocking?");
        assert!(intent.entities.is_empty());
        assert_eq!(intent.complexity, Complexity::Complex);
        assert!(intent.target_kinds.contains(&NodeKind::Approval));
        assert!(intent.target_kinds.contains(&NodeKind::Thread));

        let intent = classify("Who owns the payments API and the \"Ledger Core\" service?");
        let texts: Vec<&str> = intent.entities.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["payments", "Ledger Core"]);
        assert_eq!(intent.entities[0].kind_hint, Some(NodeKind::ApiSpec));
        assert_eq!(intent.complexity, Complexity::Moderate);

        let intent = classify("What breaks if we change POST /v1/charges for ticket PAY-123?");
        let texts: Vec<&str> = intent.entities.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["POST /v1/charges", "PAY-123"]);
        assert_eq!(intent.entities[0].kind_hint, Some(NodeKind::ApiEndpoint));
        assert_eq!(intent.entities[1].kind_hint, Some(NodeKind::Ticket));

        let intent = classify("Compare team alpha, team beta and team gamma");
        assert_eq!(intent.entities.len(), 3);
        assert_eq!(intent.complexity, Complexity::Complex);
        assert_eq!(
            classify("Summarise the status of PAY-1").complexity,
            Complexity::Complex
        );
        assert_eq!(classify("").kind, IntentKind::General);
    }

    #[test]
    fn model_output_is_repaired() {
        let query = "Who owns the payments API?";
        // Clean output.
        let clean = r#"{"intent":"ownership","entities":[{"text":"payments","kind":"api_spec"}],"target_kinds":["team"],"complexity":"simple","confidence":0.9}"#;
        let intent = parse_intent(clean, query).unwrap();
        assert_eq!(intent.kind, IntentKind::Ownership);
        assert_eq!(
            intent.entities,
            vec![EntityMention::with_kind("payments", NodeKind::ApiSpec)]
        );
        assert_eq!(intent.target_kinds, vec![NodeKind::Team]);
        assert_eq!(intent.classifier, ClassifierKind::Model);
        assert!((intent.confidence - 0.9).abs() < 1e-6);

        // Fenced, with prose, sloppy labels, unknown enum values, a hallucinated entity and an
        // out-of-range confidence.
        let fenced = "Sure! Here you go:\n```json\n{\"intent\": \"Owner\", \"entities\": [\"payments\", \"billing-core\"], \"target_kinds\": [\"squad\", \"Team\", \"user\"], \"complexity\": \"trivial\", \"confidence\": 7}\n```\nHope that helps {really}.";
        let intent = parse_intent(fenced, query).unwrap();
        assert_eq!(intent.kind, IntentKind::Ownership);
        assert_eq!(intent.entities, vec![EntityMention::new("payments")]);
        assert_eq!(intent.target_kinds, vec![NodeKind::Team, NodeKind::Person]);
        assert_eq!(intent.complexity, Complexity::Simple);
        assert_eq!(intent.confidence, 1.0);

        // Truncated mid-string: the intent survives, the rest comes from the heuristic.
        let partial = r#"{"intent": "justification", "entities": [{"text": "paym"#;
        let intent = parse_intent(partial, "Why is the payments API blocked?").unwrap();
        assert_eq!(intent.kind, IntentKind::Justification);
        assert_eq!(intent.entities[0].text, "paym");
        assert_eq!(intent.complexity, Complexity::Complex);
        assert_eq!(
            intent.target_kinds,
            default_target_kinds(IntentKind::Justification)
        );

        // Missing fields and an unknown intent: heuristic fills in, with reduced confidence.
        let unknown = r#"{"intent": "banana", "entities": ["payments"], "confidence": 0.99}"#;
        let intent = parse_intent(unknown, query).unwrap();
        assert_eq!(intent.kind, IntentKind::Ownership);
        assert!(intent.confidence <= 0.5);
        let bare = r#"{"intent": "status"}"#;
        let intent = parse_intent(bare, "Is PAY-9 done?").unwrap();
        assert_eq!(intent.kind, IntentKind::Status);
        assert_eq!(intent.entities[0].text, "PAY-9");

        // Not JSON at all, but the label is recoverable.
        let intent = parse_intent("intent: how-to (confidence high)", "How to deploy?").unwrap();
        assert_eq!(intent.kind, IntentKind::HowTo);

        // Garbage.
        for garbage in [
            "",
            "I cannot help with that.",
            "{}",
            "[1, 2]",
            "{\"intent\": 5}",
            "}{",
        ] {
            assert_eq!(parse_intent(garbage, query), None, "{garbage:?}");
        }
    }

    #[test]
    fn request_carries_schema_and_examples() {
        let request = intent_request("  Who owns API X?  ", Some("q-1"));
        assert_eq!(request.purpose, CallPurpose::Classify);
        assert_eq!(request.query_id.as_deref(), Some("q-1"));
        assert_eq!(request.messages[0].content, "Q: Who owns API X?");
        assert!(request.system.as_deref().unwrap().contains("ownership"));
        let ResponseFormat::JsonSchema(schema) = &request.response_format else {
            panic!("classification must request schema-constrained JSON");
        };
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["intent"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            7
        );
    }
}
