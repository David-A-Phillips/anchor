//! API approval portal exports: API definitions (OpenAPI) and their approval records.
//!
//! # Accepted input
//!
//! A directory (scanned recursively, hidden entries skipped) of `*.yaml`, `*.yml` and `*.json`
//! files. Each file is one of:
//!
//! * an **API definition**: an OpenAPI 3.x or Swagger 2 document (it has an `openapi` or
//!   `swagger` field);
//! * a **portal record**, or a list of them: a top-level array, or `{ "apis": [...] }` /
//!   `{ "records": [...] }`.
//!
//! A portal record (YAML shown; JSON is equivalent):
//!
//! ```yaml
//! id: API-42                    # portal identifier (alias of the API)
//! name: payments-api            # canonical name; falls back to id, then title
//! title: Payments API
//! version: "2.1"
//! description: Charges and refunds.
//! status: approved              # draft | in_review | approved | rejected | deprecated
//! owner_team: Payments Platform
//! owners:                       # people by default; teams as "@org/team", "team:<name>",
//!   - Alice Smith <alice@example.com>     # or { team: <name> }; people may also be
//!   - team:Billing                        # { name, email, handle }
//! spec: specs/payments.yaml     # path relative to the record file (or to the export root),
//!                               # or an inline definition object
//! implemented_by: [payments-service]
//! consumers: [checkout-web]
//! supersedes: legacy-payments   # name, title or portal id of the replaced API (or a list)
//! url: https://portal.example.com/apis/API-42
//! approvals:
//!   - id: APR-17
//!     approver: team:Security   # same forms as owners; the owner team's name is a team
//!     decision: rejected        # approved | rejected | changes_requested | pending
//!     blocking: true            # default: true for a rejection, false otherwise
//!     reason: Card numbers must not appear in query strings.
//!     decided_at: 2026-01-05T10:00:00Z
//!     scope: POST /v1/charges   # "<METHOD> <route>" or an operationId; absent = whole API
//! ```
//!
//! Only a name (`name`, `id` or `title`) is required. A record without `spec` is paired with the
//! definition whose `info.title` or file name matches the record's name, id or title. Definitions
//! that no record refers to are ingested on their own (see [`parse_openapi`]).
//!
//! # Output
//!
//! One [`Unit`] of kind [`UnitKind::ApiSpec`] per API, keyed
//! `keys::unit(source, UnitKind::ApiSpec, <slug of the API name>)`:
//!
//! * the ApiSpec node `keys::api_spec(name)`: aliases name, title and portal id; attributes
//!   `version`, `status`, `title`, `description` (first line), `url`;
//! * one ApiEndpoint node per operation, `keys::api_endpoint(name, method, route)`: aliases
//!   `"<METHOD> <route>"`, the same with the base path prefixed, and the operationId; attributes
//!   `method`, `route`, `description`, `deprecated`, `operation_id`, `tags`, `parameters`
//!   (`"<in>:<name>"`, path-level and operation-level merged); `api --Contains--> endpoint`;
//! * `owner --Owns--> api` for `owner_team` and `owners` (asserted with `ingest::party`);
//! * `Lookup([Service]) --Implements--> api`, `Lookup([Service]) --Consumes--> api`,
//!   `api --Supersedes--> Lookup([ApiSpec])`;
//! * one Approval node per approval, `keys::approval(source, id)`: attributes `decision`,
//!   `blocking` (effective: declared blocking and not approved), `reason`, `created_at`, `scope`;
//!   `approval --Approves--> target`, `approval --DecidedBy--> approver`, and
//!   `approval --Blocks--> target` with `reason` when blocking; the reason as a Decision chunk;
//! * Description chunks for the API and each operation, and `Mentions` edges from them.
//!
//! All functions here block (file I/O and parsing): call them from a blocking thread.

mod openapi;

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;
use walkdir::WalkDir;

use self::openapi::{Definition, parse_document, read_definition, scalar};
use super::mentions::mention_edges;
use super::party::{self, PersonIdentity};
use super::{IngestOutput, IngestWarning};
use crate::base::time::parse_timestamp_ms;
use crate::base::{
    ChunkKind, EdgeAssertion, EdgeKind, Error, Lookup, NodeAssertion, NodeKey, NodeKind, Result,
    SourceId, Unit, UnitKind, attrs, keys,
};

/// Longest `description` attribute kept on a node; the full text lives in the chunk.
const SHORT_DESCRIPTION_CHARS: usize = 240;

/// Tuning of API portal ingestion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiPortalOptions {
    /// Files larger than this are skipped with a warning.
    pub max_file_bytes: u64,
}

impl Default for ApiPortalOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: 32 * 1024 * 1024,
        }
    }
}

/// What is known about an API besides its operations.
#[derive(Default)]
struct ApiMeta {
    name: String,
    id: Option<String>,
    title: Option<String>,
    version: Option<String>,
    description: Option<String>,
    status: Option<String>,
    url: Option<String>,
}

fn short(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    match line.char_indices().nth(SHORT_DESCRIPTION_CHARS) {
        Some((cut, _)) => format!("{}...", &line[..cut]),
        None => line.to_owned(),
    }
}

/// A scalar or a list of scalars as strings.
fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(|item| scalar(Some(item))).collect(),
        other => scalar(other).into_iter().collect(),
    }
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| text[prefix.len()..].trim())
}

/// A person or team as records name them (see the module documentation). `teams` holds the
/// slugs of names already known to be teams.
fn party_node(value: &Value, teams: &BTreeSet<String>) -> Option<NodeAssertion> {
    match value {
        Value::String(text) => {
            let text = text.trim();
            if let Some(team) = strip_prefix_ignore_case(text, "team:") {
                party::team_assertion(team)
            } else if let Some(person) = strip_prefix_ignore_case(text, "person:") {
                PersonIdentity::parse(person).assertion()
            } else if teams.contains(&keys::slugify(text)) {
                party::team_assertion(text)
            } else {
                party::party_assertion(text)
            }
        }
        Value::Object(object) => {
            let field = |names: &[&str]| names.iter().find_map(|name| scalar(object.get(*name)));
            let is_team =
                field(&["kind", "type"]).is_some_and(|kind| kind.eq_ignore_ascii_case("team"));
            if let Some(team) = field(&["team"]) {
                party::team_assertion(&team)
            } else if is_team {
                party::team_assertion(&field(&["name"])?)
            } else {
                PersonIdentity::new(
                    field(&["name", "displayName"]).as_deref(),
                    field(&["handle", "username", "login"]).as_deref(),
                    field(&["email", "emailAddress"]).as_deref(),
                )
                .assertion()
            }
        }
        _ => None,
    }
}

/// Builds the nodes, containment edges and text of an API. Returns the unit (not yet canonical)
/// and the key of its ApiSpec node.
fn api_unit(source: &SourceId, meta: &ApiMeta, definition: Option<&Definition>) -> (Unit, NodeKey) {
    let slug = keys::slugify(&meta.name);
    let api = keys::api_spec(&meta.name);
    let mut unit = Unit::new(
        keys::unit(source, UnitKind::ApiSpec, &slug),
        source.clone(),
        UnitKind::ApiSpec,
    );
    let from_definition = |pick: fn(&Definition) -> &Option<String>| {
        definition.and_then(|definition| pick(definition).clone())
    };
    let title = meta.title.clone().or_else(|| from_definition(|d| &d.title));
    let version = meta
        .version
        .clone()
        .or_else(|| from_definition(|d| &d.version));
    let mut descriptions: Vec<String> = Vec::new();
    for text in [
        meta.description.clone(),
        from_definition(|d| &d.description),
    ]
    .into_iter()
    .flatten()
    {
        if !descriptions.contains(&text) {
            descriptions.push(text);
        }
    }

    let mut node = NodeAssertion::new(
        NodeKind::ApiSpec,
        api.clone(),
        title.as_deref().unwrap_or(&meta.name),
    )
    .alias(meta.name.as_str())
    .attr_opt(attrs::VERSION, version)
    .attr_opt(attrs::STATUS, meta.status.clone())
    .attr_opt(attrs::URL, meta.url.clone())
    .attr_opt(
        attrs::DESCRIPTION,
        descriptions.first().map(|text| short(text)),
    );
    if let Some(title) = &title {
        node = node
            .alias(title.as_str())
            .attr(attrs::TITLE, title.as_str());
    }
    if let Some(id) = &meta.id {
        node = node.alias(id.as_str());
    }
    unit.add_node(node);
    for text in descriptions {
        unit.edges.extend(mention_edges(&api, &text));
        unit.add_chunk(&api, ChunkKind::Description, text);
    }

    for operation in definition.into_iter().flat_map(|d| &d.operations) {
        let label = operation.label();
        let endpoint = keys::api_endpoint(&meta.name, &operation.method, &operation.route);
        let mut texts: Vec<&str> = Vec::new();
        for text in [&operation.summary, &operation.description]
            .into_iter()
            .flatten()
        {
            if !texts.contains(&text.as_str()) {
                texts.push(text);
            }
        }
        let mut node = NodeAssertion::new(NodeKind::ApiEndpoint, endpoint.clone(), label.as_str())
            .alias(label.as_str())
            .attr(attrs::METHOD, operation.method.as_str())
            .attr(attrs::ROUTE, operation.route.as_str())
            .attr("deprecated", operation.deprecated)
            .attr_opt(attrs::DESCRIPTION, texts.first().map(|text| short(text)));
        if let Some(base) = definition.and_then(|d| d.base_path.as_deref()) {
            // Code and logs usually show the full path, the definition the path below the base.
            node = node.alias(format!("{} {base}{}", operation.method, operation.route));
        }
        if let Some(operation_id) = &operation.operation_id {
            node = node
                .alias(operation_id.as_str())
                .attr("operation_id", operation_id.as_str());
        }
        if !operation.tags.is_empty() {
            node = node.attr("tags", operation.tags.clone());
        }
        if !operation.parameters.is_empty() {
            node = node.attr("parameters", operation.parameters.clone());
        }
        unit.add_node(node);
        unit.add_edge(EdgeAssertion::new(&api, EdgeKind::Contains, &endpoint));
        let text = texts.join("\n\n");
        unit.edges.extend(mention_edges(&endpoint, &text));
        unit.add_chunk(&endpoint, ChunkKind::Description, text);
    }
    (unit, api)
}

fn normalize_status(raw: &str) -> String {
    keys::slugify(raw).replace('-', "_")
}

fn normalize_decision(raw: Option<&str>) -> Option<&'static str> {
    Some(match keys::slugify(raw.unwrap_or("pending")).as_str() {
        "approved" | "approve" | "accepted" | "accept" => attrs::DECISION_APPROVED,
        "rejected" | "reject" | "declined" | "denied" => attrs::DECISION_REJECTED,
        "changes-requested" | "request-changes" | "needs-changes" | "changes-required" => {
            attrs::DECISION_CHANGES_REQUESTED
        }
        "pending" | "in-review" | "open" | "" => attrs::DECISION_PENDING,
        _ => return None,
    })
}

/// Resolves an approval scope to the node it is about. `Err` carries the API key to fall back to
/// when the scope names an operation the definition does not have.
fn scope_target(
    scope: Option<&str>,
    meta: &ApiMeta,
    api: &NodeKey,
    definition: Option<&Definition>,
) -> std::result::Result<NodeKey, NodeKey> {
    let Some(scope) = scope else {
        return Ok(api.clone());
    };
    let slug = keys::slugify(scope);
    let names_api = [Some(&meta.name), meta.id.as_ref(), meta.title.as_ref()]
        .into_iter()
        .flatten()
        .any(|name| keys::slugify(name) == slug);
    if names_api || matches!(slug.as_str(), "" | "api" | "all" | "whole" | "whole-api") {
        return Ok(api.clone());
    }
    let mut words = scope.split_whitespace();
    let as_label = match (words.next(), words.next(), words.next()) {
        (Some(method), Some(route), None) => {
            Some(format!("{} {route}", method.to_ascii_uppercase()))
        }
        _ => None,
    };
    definition
        .into_iter()
        .flat_map(|definition| &definition.operations)
        .find(|operation| {
            operation.operation_id.as_deref() == Some(scope)
                || as_label.as_deref() == Some(operation.label().as_str())
        })
        .map(|operation| keys::api_endpoint(&meta.name, &operation.method, &operation.route))
        .ok_or_else(|| api.clone())
}

/// Everything an approval needs to know about the API it belongs to.
struct ApprovalContext<'a> {
    source: &'a SourceId,
    location: &'a str,
    meta: &'a ApiMeta,
    api: &'a NodeKey,
    definition: Option<&'a Definition>,
    teams: &'a BTreeSet<String>,
}

fn add_approval(
    unit: &mut Unit,
    context: &ApprovalContext<'_>,
    id: &str,
    approval: &Value,
    warnings: &mut Vec<IngestWarning>,
) {
    let mut warn = |message: String| {
        warnings.push(IngestWarning::new(
            context.location,
            format!("approval `{id}`: {message}"),
        ));
    };
    let raw_decision = scalar(approval.get("decision"));
    let decision = normalize_decision(raw_decision.as_deref()).unwrap_or_else(|| {
        warn(format!(
            "unknown decision `{}`, treated as pending",
            raw_decision.as_deref().unwrap_or_default()
        ));
        attrs::DECISION_PENDING
    });
    let declared_blocking = match approval.get("blocking") {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => matches!(text.trim().to_lowercase().as_str(), "true" | "yes"),
        // A rejection that does not say otherwise stands in the way.
        _ => decision == attrs::DECISION_REJECTED,
    };
    let blocking = declared_blocking && decision != attrs::DECISION_APPROVED;
    let reason = scalar(approval.get("reason")).or_else(|| scalar(approval.get("comment")));
    let decided_at =
        ["decided_at", "created_at"]
            .iter()
            .find_map(|field| match approval.get(*field)? {
                Value::Number(number) => number.as_i64(),
                other => parse_timestamp_ms(&scalar(Some(other))?),
            });
    let scope = scalar(approval.get("scope"));
    let target = scope_target(
        scope.as_deref(),
        context.meta,
        context.api,
        context.definition,
    )
    .unwrap_or_else(|api| {
        warn(format!(
            "scope `{}` matches no operation of the definition; applied to the whole API",
            scope.as_deref().unwrap_or_default()
        ));
        api
    });
    let approver = approval
        .get("approver")
        .and_then(|approver| party_node(approver, context.teams))
        .or_else(|| party::team_assertion(&scalar(approval.get("approver_team"))?));

    let key = keys::approval(context.source, id);
    let name = match &approver {
        Some(approver) => format!("{id}: {decision} by {}", approver.name),
        None => format!("{id}: {decision}"),
    };
    unit.add_node(
        NodeAssertion::new(NodeKind::Approval, key.clone(), name)
            .attr(attrs::DECISION, decision)
            .attr(attrs::BLOCKING, blocking)
            .attr_opt(attrs::REASON, reason.clone())
            .attr_opt(attrs::CREATED_AT, decided_at)
            .attr_opt("scope", scope),
    );
    unit.add_edge(EdgeAssertion::new(&key, EdgeKind::Approves, &target));
    if let Some(approver) = approver {
        let approver = unit.add_node(approver);
        unit.add_edge(EdgeAssertion::new(&key, EdgeKind::DecidedBy, approver));
    }
    if blocking {
        let mut edge = EdgeAssertion::new(&key, EdgeKind::Blocks, &target);
        if let Some(reason) = &reason {
            edge = edge.attr(attrs::REASON, reason.as_str());
        }
        unit.add_edge(edge);
    }
    if let Some(reason) = reason {
        unit.edges.extend(mention_edges(&key, &reason));
        unit.add_chunk(&key, ChunkKind::Decision, reason);
    }
}

fn record_unit(
    source: &SourceId,
    location: &str,
    record: &Value,
    external: Option<&Value>,
    out: &mut IngestOutput,
) {
    if !record.is_object() {
        out.warn(location, "portal record is not an object");
        return;
    }
    let field = |name: &str| scalar(record.get(name));
    let id = field("id");
    let title = field("title");
    let Some(name) = field("name")
        .or_else(|| id.clone())
        .or_else(|| title.clone())
        .filter(|name| !keys::slugify(name).is_empty())
    else {
        out.warn(
            location,
            "portal record has no usable `name`, `id` or `title`",
        );
        return;
    };
    let status = field("status").map(|raw| {
        let status = normalize_status(&raw);
        let known = ["draft", "in_review", "approved", "rejected", "deprecated"];
        if !known.contains(&status.as_str()) {
            out.warn(
                location,
                format!("unknown status `{raw}` kept as `{status}`"),
            );
        }
        status
    });
    let meta = ApiMeta {
        name,
        id,
        title,
        version: field("version"),
        description: field("description"),
        status,
        url: field("url"),
    };

    let spec = record.get("spec").or_else(|| record.get("definition"));
    let document = match spec {
        Some(inline @ Value::Object(_)) => Some(inline),
        _ => external,
    };
    let definition = match document.map(read_definition) {
        Some(Ok(definition)) => Some(definition),
        Some(Err(message)) => {
            out.warn(
                location,
                format!("API definition of `{}`: {message}", meta.name),
            );
            None
        }
        None => {
            out.warn(
                location,
                match scalar(spec) {
                    Some(path) => format!(
                        "API definition `{path}` of `{}` is missing or unreadable; ingested without operations",
                        meta.name
                    ),
                    None => format!(
                        "no API definition found for `{}`; ingested without operations",
                        meta.name
                    ),
                },
            );
            None
        }
    };

    let (mut unit, api) = api_unit(source, &meta, definition.as_ref());

    let mut teams = BTreeSet::new();
    if let Some(team) = field("owner_team").and_then(|team| party::team_assertion(&team)) {
        teams.insert(keys::slugify(&team.name));
        let team = unit.add_node(team);
        unit.add_edge(EdgeAssertion::new(team, EdgeKind::Owns, &api));
    }
    let owners = match record.get("owners") {
        Some(Value::Array(owners)) => owners.as_slice(),
        Some(owner) => std::slice::from_ref(owner),
        None => &[],
    };
    for owner in owners {
        match party_node(owner, &teams) {
            Some(owner) => {
                let owner = unit.add_node(owner);
                unit.add_edge(EdgeAssertion::new(owner, EdgeKind::Owns, &api));
            }
            None => out.warn(location, format!("unusable owner `{owner}`")),
        }
    }
    for (field, kind) in [
        ("implemented_by", EdgeKind::Implements),
        ("consumers", EdgeKind::Consumes),
    ] {
        for service in strings(record.get(field)) {
            let service = Lookup::new(&[NodeKind::Service], service);
            unit.add_edge(EdgeAssertion::new(service, kind, &api));
        }
    }
    for superseded in strings(record.get("supersedes")) {
        if keys::api_spec(&superseded) != api {
            let superseded = Lookup::new(&[NodeKind::ApiSpec], superseded);
            unit.add_edge(EdgeAssertion::new(&api, EdgeKind::Supersedes, superseded));
        }
    }

    let context = ApprovalContext {
        source,
        location,
        meta: &meta,
        api: &api,
        definition: definition.as_ref(),
        teams: &teams,
    };
    let approvals = match record.get("approvals") {
        Some(Value::Array(approvals)) => approvals.as_slice(),
        Some(Value::Null) | None => &[],
        Some(_) => {
            out.warn(location, "`approvals` is not a list");
            &[]
        }
    };
    let mut seen = BTreeSet::new();
    for (index, approval) in approvals.iter().enumerate() {
        if !approval.is_object() {
            out.warn(location, format!("approval #{index} is not an object"));
            continue;
        }
        // Position is the only stable identity an id-less approval has.
        let id = scalar(approval.get("id"))
            .unwrap_or_else(|| format!("{}-{}", keys::slugify(&meta.name), index + 1));
        if !seen.insert(id.clone()) {
            out.warn(location, format!("duplicate approval id `{id}` ignored"));
            continue;
        }
        add_approval(&mut unit, &context, &id, approval, &mut out.warnings);
    }
    out.units.push(unit.finish());
}

/// Builds the unit of one portal record (the per-record entry point).
///
/// `definition` is the parsed OpenAPI/Swagger document the record refers to, when the caller has
/// it; an inline `spec` object in the record takes precedence. `location` labels warnings.
///
/// Never fails. A record that is not an object or has no name yields no unit and one warning. A
/// record without a usable definition still yields its ApiSpec node (ownership, approvals and so
/// on included) plus a warning; approvals scoped to an unknown operation are applied to the whole
/// API with a warning.
pub fn parse_portal_record(
    source: &SourceId,
    location: &str,
    record: &Value,
    definition: Option<&Value>,
) -> IngestOutput {
    let mut out = IngestOutput::default();
    record_unit(source, location, record, definition, &mut out);
    out
}

fn standalone_unit(source: &SourceId, name_hint: &str, definition: &Definition) -> Result<Unit> {
    let name = [Some(name_hint), definition.title.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|name| !keys::slugify(name).is_empty())
        .ok_or_else(|| Error::parse("API definition has no `info.title` and no name was given"))?;
    let meta = ApiMeta {
        name: name.to_owned(),
        ..ApiMeta::default()
    };
    Ok(api_unit(source, &meta, Some(definition)).0.finish())
}

/// Builds the unit of a standalone OpenAPI 3.x / Swagger 2 definition (YAML or JSON text) that
/// has no portal record, such as a definition found inside a repository.
///
/// The API is named `name_hint` when it is not blank (pass the owning service's or the portal's
/// name for the API), otherwise `info.title`. The title is always registered as an alias. Since
/// ApiSpec nodes merge by alias, do not pass a generic file name such as `openapi` as the hint.
///
/// # Errors
///
/// [`Error::Parse`] when the text is not valid YAML/JSON, is not an OpenAPI/Swagger document, or
/// no name can be determined. Malformed paths and operations inside a valid document are skipped.
pub fn parse_openapi(source: &SourceId, name_hint: &str, text: &str) -> Result<Unit> {
    let document = parse_document(text).map_err(Error::parse)?;
    let definition = read_definition(&document).map_err(Error::parse)?;
    standalone_unit(source, name_hint, &definition)
}

/// Lexically resolves `relative` against the directory `base` (both `/`-separated and relative
/// to the export root). `None` when the result would leave the root.
fn join_relative(base: &str, relative: &str) -> Option<String> {
    let relative = relative.replace('\\', "/");
    if relative.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn parent_dir(location: &str) -> &str {
    location.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn file_stem(location: &str) -> &str {
    let name = location.rsplit('/').next().unwrap_or(location);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// Finds the definition file a record refers to: by `spec` path, else by matching names.
fn find_definition<'a>(
    definitions: &'a BTreeMap<String, Value>,
    record_file: &str,
    record: &Value,
) -> Option<&'a str> {
    let spec = record.get("spec").or_else(|| record.get("definition"));
    if let Some(path) = spec.and_then(Value::as_str) {
        return [parent_dir(record_file), ""]
            .iter()
            .filter_map(|base| join_relative(base, path))
            .find_map(|candidate| definitions.get_key_value(&candidate))
            .map(|(location, _)| location.as_str());
    }
    if spec.is_some_and(|spec| !spec.is_null()) {
        return None;
    }
    let wanted: BTreeSet<String> = ["name", "id", "title"]
        .iter()
        .filter_map(|field| scalar(record.get(*field)))
        .map(|name| keys::slugify(&name))
        .filter(|slug| !slug.is_empty())
        .collect();
    definitions
        .iter()
        .find(|(location, document)| {
            let title = scalar(document.get("info").and_then(|info| info.get("title")));
            title.is_some_and(|title| wanted.contains(&keys::slugify(&title)))
                || wanted.contains(&keys::slugify(file_stem(location)))
        })
        .map(|(location, _)| location.as_str())
}

/// Ingests an API portal export directory (see the module documentation for the layout).
///
/// Unreadable or malformed files, bad records and missing definitions become warnings; only an
/// unusable `dir` is an error ([`Error::NotFound`]). When two APIs end up with the same name the
/// first one (records before standalone definitions, files in path order) is kept and a warning
/// is recorded. Units are returned in canonical form, ordered by key, so the same directory
/// always yields the same output.
pub fn scan_api_portal(
    dir: &Path,
    source: &SourceId,
    options: &ApiPortalOptions,
) -> Result<IngestOutput> {
    if !dir.is_dir() {
        return Err(Error::not_found(format!(
            "API portal export directory `{}` does not exist or is not a directory",
            dir.display()
        )));
    }
    let mut out = IngestOutput::default();
    let mut definitions: BTreeMap<String, Value> = BTreeMap::new();
    let mut records: Vec<(String, String, Value)> = Vec::new();

    let walker = WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
        });
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                out.warn(dir.display().to_string(), format!("cannot read: {err}"));
                continue;
            }
        };
        let path = entry.path();
        let wanted = path.extension().is_some_and(|ext| {
            ["yaml", "yml", "json"]
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        });
        if !entry.file_type().is_file() || !wanted {
            continue;
        }
        let location = path
            .strip_prefix(dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let text = match entry.metadata() {
            Ok(meta) if meta.len() > options.max_file_bytes => Err("file is too large".to_owned()),
            Ok(_) => std::fs::read_to_string(path).map_err(|err| format!("cannot read: {err}")),
            Err(err) => Err(format!("cannot read: {err}")),
        };
        let document = match text.and_then(|text| parse_document(&text)) {
            Ok(document) => document,
            Err(message) => {
                out.warn(location, message);
                continue;
            }
        };
        if openapi::is_definition(&document) {
            definitions.insert(location, document);
            continue;
        }
        let list = match &document {
            Value::Array(items) => Some(items),
            Value::Object(object) => ["apis", "records"]
                .iter()
                .find_map(|field| object.get(*field).and_then(Value::as_array)),
            _ => None,
        };
        match list {
            Some(items) => {
                for (index, item) in items.iter().enumerate() {
                    records.push((
                        location.clone(),
                        format!("{location}#{index}"),
                        item.clone(),
                    ));
                }
            }
            None if document.is_object() => {
                records.push((location.clone(), location, document));
            }
            None => out.warn(location, "neither an API definition nor a portal record"),
        }
    }

    let mut units: BTreeMap<String, Unit> = BTreeMap::new();
    let mut keep = |parsed: IngestOutput, location: &str, out: &mut IngestOutput| {
        out.warnings.extend(parsed.warnings);
        for unit in parsed.units {
            match units.entry(unit.key.as_str().to_owned()) {
                Entry::Vacant(slot) => {
                    slot.insert(unit);
                }
                Entry::Occupied(slot) => out.warn(
                    location,
                    format!("API `{}` is already defined elsewhere; ignored", slot.key()),
                ),
            }
        }
    };

    let mut used: BTreeSet<&str> = BTreeSet::new();
    for (file, location, record) in &records {
        let definition = find_definition(&definitions, file, record);
        used.extend(definition);
        let document = definition.and_then(|definition| definitions.get(definition));
        let parsed = parse_portal_record(source, location, record, document);
        keep(parsed, location, &mut out);
    }
    for (location, document) in &definitions {
        if used.contains(location.as_str()) {
            continue;
        }
        let mut parsed = IngestOutput::default();
        // The file name is only a last resort: `openapi.yaml` names nothing.
        let unit = read_definition(document)
            .map_err(Error::parse)
            .and_then(|definition| {
                let hint = match definition.title {
                    Some(_) => "",
                    None => file_stem(location),
                };
                standalone_unit(source, hint, &definition)
            });
        match unit {
            Ok(unit) => parsed.units.push(unit),
            Err(err) => parsed.warn(location.as_str(), err.to_string()),
        }
        keep(parsed, location, &mut out);
    }
    out.units = units.into_values().collect();
    Ok(out)
}

#[cfg(test)]
mod tests;
