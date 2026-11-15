//! Reading OpenAPI 3.x and Swagger 2 documents (YAML or JSON) into a neutral shape.

use std::collections::BTreeSet;

use serde_json::{Number, Value};

/// HTTP methods that may appear as keys of a path item.
const METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// The parts of an API definition that end up in the graph.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Definition {
    pub title: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Path prefix under which the routes are served (`basePath`, or the path of the first
    /// server URL), when there is one.
    pub base_path: Option<String>,
    pub operations: Vec<Operation>,
}

/// One operation: a method on a route.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Operation {
    /// Upper-cased HTTP method.
    pub method: String,
    /// Route template exactly as written in `paths`.
    pub route: String,
    pub operation_id: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub deprecated: bool,
    pub tags: Vec<String>,
    /// Path-level and operation-level parameters as `"<in>:<name>"`, sorted.
    pub parameters: Vec<String>,
}

impl Operation {
    /// The lookup alias of the endpoint: `"<METHOD> <route>"`.
    pub fn label(&self) -> String {
        format!("{} {}", self.method, self.route)
    }
}

/// Text of a scalar: YAML happily turns `version: 1.0` or `swagger: 2.0` into numbers.
pub(super) fn scalar(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        }
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn yaml_to_json(value: serde_yaml_ng::Value) -> Value {
    use serde_yaml_ng::Value as Yaml;
    match value {
        Yaml::Null => Value::Null,
        Yaml::Bool(flag) => Value::Bool(flag),
        Yaml::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Value::from(integer)
            } else if let Some(integer) = number.as_u64() {
                Value::from(integer)
            } else {
                // NaN and infinities have no JSON form.
                number
                    .as_f64()
                    .and_then(Number::from_f64)
                    .map_or(Value::Null, Value::Number)
            }
        }
        Yaml::String(text) => Value::String(text),
        Yaml::Sequence(items) => Value::Array(items.into_iter().map(yaml_to_json).collect()),
        Yaml::Mapping(mapping) => Value::Object(
            mapping
                .into_iter()
                .filter_map(|(key, value)| {
                    // Response codes and the like are written as bare integers (`200:`).
                    let key = match key {
                        Yaml::String(text) => text,
                        Yaml::Number(number) => number.to_string(),
                        Yaml::Bool(flag) => flag.to_string(),
                        _ => return None,
                    };
                    Some((key, yaml_to_json(value)))
                })
                .collect(),
        ),
        Yaml::Tagged(tagged) => yaml_to_json(tagged.value),
    }
}

/// Parses a JSON or YAML document into a JSON value.
pub(super) fn parse_document(text: &str) -> Result<Value, String> {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).map_err(|err| format!("invalid JSON: {err}"));
    }
    serde_yaml_ng::from_str::<serde_yaml_ng::Value>(trimmed)
        .map(yaml_to_json)
        .map_err(|err| format!("invalid YAML: {err}"))
}

/// `true` when the document declares itself an OpenAPI 3.x or Swagger 2 definition.
pub(super) fn is_definition(document: &Value) -> bool {
    scalar(document.get("openapi")).is_some() || scalar(document.get("swagger")).is_some()
}

/// Follows a local `$ref` (`#/components/parameters/Limit`); anything else is returned as is.
fn resolve<'a>(document: &'a Value, value: &'a Value) -> &'a Value {
    let mut current = value;
    // Bounded so that a reference cycle cannot loop forever.
    for _ in 0..8 {
        let target = current
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix('#'))
            .and_then(|pointer| document.pointer(pointer));
        match target {
            Some(target) => current = target,
            None => break,
        }
    }
    current
}

fn parameters(document: &Value, holder: &Value, into: &mut BTreeSet<String>) {
    for parameter in holder
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let parameter = resolve(document, parameter);
        if let Some(name) = scalar(parameter.get("name")) {
            let location = scalar(parameter.get("in")).unwrap_or_else(|| "query".to_owned());
            into.insert(format!("{location}:{name}"));
        }
    }
}

fn base_path(document: &Value) -> Option<String> {
    let raw = scalar(document.get("basePath")).or_else(|| {
        let url = scalar(document.get("servers")?.as_array()?.first()?.get("url"))?;
        let path = match url.find("://") {
            Some(scheme) => {
                let rest = &url[scheme + 3..];
                rest.find('/').map(|slash| rest[slash..].to_owned())?
            }
            None => url,
        };
        Some(path)
    })?;
    let path = raw.trim_end_matches('/');
    (path.starts_with('/') && !path.contains('{')).then(|| path.to_owned())
}

/// Extracts the definition. Fails only when the document is not an OpenAPI/Swagger object;
/// malformed paths and operations are skipped.
pub(super) fn read_definition(document: &Value) -> Result<Definition, String> {
    if !document.is_object() || !is_definition(document) {
        return Err(
            "not an OpenAPI 3.x or Swagger 2 document (no `openapi` or `swagger` field)".to_owned(),
        );
    }
    let info = document.get("info").unwrap_or(&Value::Null);
    let mut definition = Definition {
        title: scalar(info.get("title")),
        version: scalar(info.get("version")),
        description: scalar(info.get("description")),
        base_path: base_path(document),
        operations: Vec::new(),
    };
    let Some(paths) = document.get("paths").and_then(Value::as_object) else {
        return Ok(definition);
    };
    for (route, item) in paths {
        let route = route.trim();
        let item = resolve(document, item);
        if !route.starts_with('/') || !item.is_object() {
            continue;
        }
        let mut shared = BTreeSet::new();
        parameters(document, item, &mut shared);
        for method in METHODS {
            let Some(operation) = item.get(*method).filter(|operation| operation.is_object())
            else {
                continue;
            };
            let mut all = shared.clone();
            parameters(document, operation, &mut all);
            definition.operations.push(Operation {
                method: method.to_ascii_uppercase(),
                route: route.to_owned(),
                operation_id: scalar(operation.get("operationId")),
                // A path item may describe all of its operations at once.
                summary: scalar(operation.get("summary")).or_else(|| scalar(item.get("summary"))),
                description: scalar(operation.get("description"))
                    .or_else(|| scalar(item.get("description"))),
                deprecated: operation.get("deprecated").and_then(Value::as_bool) == Some(true),
                tags: operation
                    .get("tags")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|tag| scalar(Some(tag)))
                    .collect(),
                parameters: all.into_iter().collect(),
            });
        }
    }
    definition
        .operations
        .sort_by(|a, b| (&a.route, &a.method).cmp(&(&b.route, &b.method)));
    Ok(definition)
}
