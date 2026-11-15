use serde_json::json;

use super::*;
use crate::base::{EdgeAssertion, NodeRef};

const PAYMENTS_YAML: &str = r##"
openapi: 3.0.3
info:
  title: Payments API
  version: 2.1
  description: |
    Charges and refunds. Backed by `Ledger::charge` in payments-service.
servers:
  - url: https://api.example.com/payments/
paths:
  /v1/charges/{id}:
    parameters:
      - name: id
        in: path
        required: true
      - $ref: '#/components/parameters/Trace'
    get:
      operationId: getCharge
      summary: Fetch a charge
      tags: [charges]
      responses:
        200:
          description: ok
    delete:
      operationId: voidCharge
      description: Voids a charge. See PAY-7.
      deprecated: true
      parameters:
        - name: force
          in: query
  /v1/charges:
    post:
      operationId: createCharge
      summary: Create a charge
      description: Creates a charge.
  x-internal: { get: {} }
  /broken: nonsense
components:
  parameters:
    Trace:
      name: X-Trace
      in: header
"##;

fn source() -> SourceId {
    SourceId::new("portal")
}

fn node<'a>(unit: &'a Unit, key: &NodeKey) -> &'a NodeAssertion {
    unit.nodes
        .iter()
        .find(|node| node.key == *key)
        .unwrap_or_else(|| panic!("no node {key}"))
}

fn edge(
    unit: &Unit,
    from: impl Into<NodeRef>,
    kind: EdgeKind,
    to: impl Into<NodeRef>,
) -> &EdgeAssertion {
    let (from, to) = (from.into(), to.into());
    unit.edges
        .iter()
        .find(|edge| edge.from == from && edge.kind == kind && edge.to == to)
        .unwrap_or_else(|| panic!("no edge {from:?} --{kind}--> {to:?}"))
}

fn has_edge(unit: &Unit, from: &NodeKey, kind: EdgeKind) -> bool {
    let from = NodeRef::Key(from.clone());
    unit.edges
        .iter()
        .any(|edge| edge.from == from && edge.kind == kind)
}

#[test]
fn openapi3_yaml_yields_api_and_endpoints() {
    let unit = parse_openapi(&source(), "", PAYMENTS_YAML).unwrap();
    assert_eq!(unit.kind, UnitKind::ApiSpec);
    assert_eq!(unit.key.as_str(), "portal/api_spec/payments-api");
    let api = keys::api_spec("Payments API");
    let spec = node(&unit, &api);
    assert_eq!(spec.name, "Payments API");
    assert_eq!(spec.aliases, vec!["Payments API"]);
    assert_eq!(spec.attrs[attrs::VERSION], "2.1");
    assert_eq!(
        spec.attrs[attrs::DESCRIPTION],
        "Charges and refunds. Backed by `Ledger::charge` in payments-service."
    );
    assert!(!spec.attrs.contains_key(attrs::STATUS));

    let endpoints: Vec<&str> = unit
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::ApiEndpoint)
        .map(|node| node.name.as_str())
        .collect();
    assert_eq!(
        endpoints,
        [
            "DELETE /v1/charges/{id}",
            "GET /v1/charges/{id}",
            "POST /v1/charges"
        ],
        "path-level `parameters`, extensions and malformed path items are not operations"
    );

    let get_key = keys::api_endpoint("Payments API", "get", "/v1/charges/{id}");
    assert_eq!(
        get_key.as_str(),
        "endpoint:payments-api:GET /v1/charges/{id}"
    );
    let get = node(&unit, &get_key);
    assert_eq!(
        get.aliases,
        vec![
            "GET /payments/v1/charges/{id}",
            "GET /v1/charges/{id}",
            "getCharge"
        ]
    );
    assert_eq!(get.attrs[attrs::METHOD], "GET");
    assert_eq!(get.attrs[attrs::ROUTE], "/v1/charges/{id}");
    assert_eq!(get.attrs[attrs::DESCRIPTION], "Fetch a charge");
    assert_eq!(get.attrs["deprecated"], false);
    assert_eq!(
        get.attrs["parameters"],
        json!(["header:X-Trace", "path:id"])
    );
    edge(&unit, &api, EdgeKind::Contains, &get_key);

    let delete_key = keys::api_endpoint("Payments API", "DELETE", "/v1/charges/{id}");
    let delete = node(&unit, &delete_key);
    assert_eq!(delete.attrs["deprecated"], true);
    assert_eq!(
        delete.attrs["parameters"],
        json!(["header:X-Trace", "path:id", "query:force"])
    );
    let ticket = Lookup::new(&[NodeKind::Ticket], "PAY-7");
    edge(&unit, &delete_key, EdgeKind::Mentions, ticket);

    let post_key = keys::api_endpoint("Payments API", "POST", "/v1/charges");
    let chunk = |key: &NodeKey| {
        let chunk = unit.chunks.iter().find(|chunk| chunk.node == *key).unwrap();
        assert_eq!(chunk.kind, ChunkKind::Description);
        chunk.text.as_str()
    };
    assert_eq!(chunk(&post_key), "Create a charge\n\nCreates a charge.");
    assert!(chunk(&api).starts_with("Charges and refunds."));
    assert!(unit.edges.iter().any(|edge| edge.kind == EdgeKind::Mentions
        && edge.from == NodeRef::Key(api.clone())
        && matches!(&edge.to, NodeRef::Lookup(lookup) if lookup.text == "payments-service")));
}

#[test]
fn json_and_yaml_definitions_are_equivalent() {
    let document = openapi::parse_document(PAYMENTS_YAML).unwrap();
    let json_text = serde_json::to_string_pretty(&document).unwrap();
    let from_json = parse_openapi(&source(), "", &json_text).unwrap();
    let from_yaml = parse_openapi(&source(), "", PAYMENTS_YAML).unwrap();
    assert_eq!(from_json, from_yaml);
    assert_eq!(
        from_json.content_hash().unwrap(),
        from_yaml.content_hash().unwrap()
    );
}

#[test]
fn swagger2_definition_uses_base_path_and_name_hint() {
    let text = r#"
swagger: 2.0
info: { title: Legacy Ledger, version: "1" }
basePath: /ledger
paths:
  /entries:
    get: { operationId: listEntries, summary: List entries }
    put: { summary: Replace entries }
"#;
    let unit = parse_openapi(&source(), "ledger-api", text).unwrap();
    assert_eq!(unit.key.as_str(), "portal/api_spec/ledger-api");
    let spec = node(&unit, &keys::api_spec("ledger-api"));
    assert_eq!(spec.name, "Legacy Ledger");
    assert_eq!(spec.aliases, vec!["Legacy Ledger", "ledger-api"]);
    let get = node(&unit, &keys::api_endpoint("ledger-api", "GET", "/entries"));
    assert_eq!(
        get.aliases,
        vec!["GET /entries", "GET /ledger/entries", "listEntries"]
    );
    node(&unit, &keys::api_endpoint("ledger-api", "PUT", "/entries"));
}

#[test]
fn malformed_definitions_are_errors() {
    for bad in [
        "",
        "just text",
        "{ \"openapi\": ",
        "a: [unclosed",
        "name: not-a-definition",
        "openapi: 3.0.0\npaths: {}",
    ] {
        let result = parse_openapi(&source(), " ", bad);
        assert!(matches!(result, Err(Error::Parse(_))), "{bad:?}");
    }
    let odd = "openapi: 3.0.0\ninfo: 7\npaths: [1]";
    let unit = parse_openapi(&source(), "odd", odd).unwrap();
    assert_eq!(unit.nodes.len(), 1);
}

fn write(dir: &Path, name: &str, text: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn portal_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "specs/payments.yaml", PAYMENTS_YAML);
    write(
        root,
        "records/payments.yaml",
        r#"
id: API-42
name: payments-api
title: Payments API
version: "3.0"
description: Portal summary.
status: In Review
owner_team: Payments Platform
owners:
  - Alice Smith <alice@example.com>
  - team:Billing
spec: ../specs/payments.yaml
implemented_by: [payments-service]
consumers: [checkout-web, mobile-app]
supersedes: legacy-payments
url: https://portal.example.com/apis/API-42
approvals:
  - id: APR-1
    approver: team:Security
    decision: rejected
    blocking: true
    reason: Card numbers must not appear in the query string of GET /v1/charges/{id}.
    decided_at: 2026-01-05T10:00:00Z
  - id: APR-2
    approver: { name: Bob Jones, email: bob@example.com }
    decision: changes_requested
    blocking: true
    reason: Needs an idempotency key.
    scope: POST /v1/charges
  - id: APR-3
    approver: Payments Platform
    decision: Approved
    blocking: true
    scope: voidCharge
  - id: APR-4
    decision: rejected
    blocking: false
    scope: PATCH /nowhere
  - id: APR-1
    decision: approved
  - id: APR-5
    decision: maybe
"#,
    );
    write(
        root,
        "records/list.json",
        &json!({ "apis": [
            { "name": "orphan-api", "spec": "missing.yaml", "status": "draft" },
            { "title": "" },
            "not a record",
            { "name": "inline-api", "spec": {
                "swagger": "2.0", "info": { "title": "Inline" },
                "paths": { "/ping": { "get": {} } }
            } },
            { "id": "API-9", "title": "Standalone Things" , "status": "deprecated" }
        ] })
        .to_string(),
    );
    write(
        root,
        "standalone.json",
        &json!({
            "openapi": "3.1.0",
            "info": { "title": "Search API" },
            "paths": { "/search": { "get": { "operationId": "search" } } }
        })
        .to_string(),
    );
    write(
        root,
        "things.yml",
        "openapi: 3.0.0\ninfo: { title: Standalone Things }\npaths: { /things: { get: {} } }\n",
    );
    write(root, "broken.yaml", "a: [unclosed");
    write(root, "scalar.json", "42");
    write(root, ".hidden/secret.yaml", "name: hidden");
    write(root, "README.md", "ignored");
    dir
}

fn scan(dir: &Path) -> IngestOutput {
    scan_api_portal(dir, &source(), &ApiPortalOptions::default()).unwrap()
}

fn unit<'a>(out: &'a IngestOutput, slug: &str) -> &'a Unit {
    let key = format!("portal/api_spec/{slug}");
    out.units
        .iter()
        .find(|unit| unit.key.as_str() == key)
        .unwrap_or_else(|| panic!("no unit {key}"))
}

#[test]
fn portal_record_asserts_ownership_and_relations() {
    let dir = portal_fixture();
    let out = scan(dir.path());
    let unit = unit(&out, "payments-api");
    let api = keys::api_spec("payments-api");
    let spec = node(unit, &api);
    assert_eq!(spec.name, "Payments API");
    assert_eq!(spec.aliases, vec!["API-42", "Payments API", "payments-api"]);
    assert_eq!(spec.attrs[attrs::STATUS], "in_review");
    assert_eq!(spec.attrs[attrs::VERSION], "3.0", "the record wins");
    assert_eq!(spec.attrs[attrs::DESCRIPTION], "Portal summary.");
    assert_eq!(
        spec.attrs[attrs::URL],
        "https://portal.example.com/apis/API-42"
    );
    let descriptions = unit.chunks.iter().filter(|chunk| chunk.node == api).count();
    assert_eq!(descriptions, 2, "portal and definition descriptions");
    assert_eq!(
        unit.nodes
            .iter()
            .filter(|node| node.kind == NodeKind::ApiEndpoint)
            .count(),
        3
    );

    edge(unit, keys::team("Payments Platform"), EdgeKind::Owns, &api);
    edge(unit, keys::team("Billing"), EdgeKind::Owns, &api);
    edge(unit, keys::person("alice"), EdgeKind::Owns, &api);
    assert_eq!(node(unit, &keys::person("alice")).name, "Alice Smith");
    let service = |name: &str| Lookup::new(&[NodeKind::Service], name);
    edge(
        unit,
        service("payments-service"),
        EdgeKind::Implements,
        &api,
    );
    edge(unit, service("checkout-web"), EdgeKind::Consumes, &api);
    edge(unit, service("mobile-app"), EdgeKind::Consumes, &api);
    let legacy = Lookup::new(&[NodeKind::ApiSpec], "legacy-payments");
    edge(unit, &api, EdgeKind::Supersedes, legacy);
}

#[test]
fn approvals_block_the_api_or_a_single_operation() {
    let dir = portal_fixture();
    let out = scan(dir.path());
    let unit = unit(&out, "payments-api");
    let api = keys::api_spec("payments-api");

    // A blocking rejection of the whole API.
    let rejected = keys::approval(&source(), "APR-1");
    let record = node(unit, &rejected);
    assert_eq!(record.kind, NodeKind::Approval);
    assert_eq!(record.name, "APR-1: rejected by Security");
    assert_eq!(record.attrs[attrs::DECISION], attrs::DECISION_REJECTED);
    assert_eq!(record.attrs[attrs::BLOCKING], true);
    assert_eq!(
        record.attrs[attrs::CREATED_AT],
        json!(1_767_607_200_000_i64)
    );
    edge(unit, &rejected, EdgeKind::Approves, &api);
    edge(unit, &rejected, EdgeKind::DecidedBy, keys::team("Security"));
    let reason = "Card numbers must not appear in the query string of GET /v1/charges/{id}.";
    assert_eq!(record.attrs[attrs::REASON], reason);
    let blocks = edge(unit, &rejected, EdgeKind::Blocks, &api);
    assert_eq!(blocks.attrs[attrs::REASON], reason);
    let chunk = unit.chunks.iter().find(|chunk| chunk.node == rejected);
    assert_eq!(chunk.unwrap().kind, ChunkKind::Decision);
    assert_eq!(chunk.unwrap().text, reason);
    let mentioned = Lookup::new(&[NodeKind::ApiEndpoint], "GET /v1/charges/{id}");
    edge(unit, &rejected, EdgeKind::Mentions, mentioned);

    // Blocking changes requested on one operation, addressed as "<METHOD> <route>".
    let changes = keys::approval(&source(), "APR-2");
    let post = keys::api_endpoint("payments-api", "POST", "/v1/charges");
    assert_eq!(
        node(unit, &changes).attrs[attrs::DECISION],
        attrs::DECISION_CHANGES_REQUESTED
    );
    assert_eq!(node(unit, &changes).attrs["scope"], "POST /v1/charges");
    edge(unit, &changes, EdgeKind::Approves, &post);
    edge(unit, &changes, EdgeKind::DecidedBy, keys::person("bob"));
    let blocks = edge(unit, &changes, EdgeKind::Blocks, &post);
    assert_eq!(blocks.attrs[attrs::REASON], "Needs an idempotency key.");

    // An approval never blocks, whatever the record says; scope by operationId; the owner
    // team's name denotes a team.
    let approved = keys::approval(&source(), "APR-3");
    let void = keys::api_endpoint("payments-api", "DELETE", "/v1/charges/{id}");
    assert_eq!(node(unit, &approved).attrs[attrs::BLOCKING], false);
    edge(unit, &approved, EdgeKind::Approves, &void);
    let owner_team = keys::team("Payments Platform");
    edge(unit, &approved, EdgeKind::DecidedBy, owner_team);
    assert!(!has_edge(unit, &approved, EdgeKind::Blocks));

    // A non-blocking rejection of an unknown operation falls back to the API, with a warning.
    let advisory = keys::approval(&source(), "APR-4");
    edge(unit, &advisory, EdgeKind::Approves, &api);
    assert!(!has_edge(unit, &advisory, EdgeKind::Blocks));
    assert!(!has_edge(unit, &advisory, EdgeKind::DecidedBy));
    let unknown = keys::approval(&source(), "APR-5");
    assert_eq!(
        node(unit, &unknown).attrs[attrs::DECISION],
        attrs::DECISION_PENDING
    );

    let messages: Vec<&str> = out
        .warnings
        .iter()
        .filter(|warning| warning.location == "records/payments.yaml")
        .map(|warning| warning.message.as_str())
        .collect();
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert!(messages[0].contains("PATCH /nowhere"));
    assert!(messages[1].contains("duplicate approval id `APR-1`"));
    assert!(messages[2].contains("unknown decision `maybe`"));
}

#[test]
fn scan_handles_missing_inline_matched_and_standalone_definitions() {
    let dir = portal_fixture();
    let out = scan(dir.path());
    let keys_found: Vec<&str> = out.units.iter().map(|unit| unit.key.as_str()).collect();
    assert_eq!(
        keys_found,
        [
            "portal/api_spec/api-9",
            "portal/api_spec/inline-api",
            "portal/api_spec/orphan-api",
            "portal/api_spec/payments-api",
            "portal/api_spec/search-api",
        ]
    );

    // A record whose definition is missing still yields the ApiSpec node.
    let orphan = unit(&out, "orphan-api");
    assert_eq!(orphan.nodes.len(), 1);
    assert_eq!(orphan.nodes[0].attrs[attrs::STATUS], "draft");
    // An inline definition.
    let inline = unit(&out, "inline-api");
    node(inline, &keys::api_endpoint("inline-api", "GET", "/ping"));
    // A record without `spec` is paired with the definition bearing its title, which is then
    // not ingested a second time.
    let matched = unit(&out, "api-9");
    node(matched, &keys::api_endpoint("API-9", "GET", "/things"));
    assert!(
        node(matched, &keys::api_spec("API-9"))
            .aliases
            .contains(&"Standalone Things".to_owned())
    );
    // A definition nobody refers to stands alone.
    let standalone = unit(&out, "search-api");
    node(
        standalone,
        &keys::api_endpoint("Search API", "GET", "/search"),
    );

    let warned: Vec<&str> = out.warnings.iter().map(|w| w.location.as_str()).collect();
    assert_eq!(
        warned,
        [
            "broken.yaml",
            "scalar.json",
            "records/list.json#0",
            "records/list.json#1",
            "records/list.json#2",
            "records/payments.yaml",
            "records/payments.yaml",
            "records/payments.yaml",
        ]
    );
    assert!(out.warnings[2].message.contains("`missing.yaml`"));

    let missing = dir.path().join("nope");
    assert!(scan_api_portal(&missing, &source(), &ApiPortalOptions::default()).is_err());
}

#[test]
fn scanning_twice_gives_identical_canonical_units() {
    let dir = portal_fixture();
    let first = scan(dir.path());
    let second = scan(dir.path());
    assert_eq!(first, second);
    for (a, b) in first.units.iter().zip(&second.units) {
        assert_eq!(a.content_hash().unwrap(), b.content_hash().unwrap());
        let mut canonical = a.clone();
        canonical.canonicalize();
        assert_eq!(&canonical, a, "units are returned in canonical form");
    }
}

#[test]
fn spec_paths_cannot_leave_the_export_root() {
    assert_eq!(
        join_relative("records", "../specs/a.yaml").as_deref(),
        Some("specs/a.yaml")
    );
    assert_eq!(join_relative("", "./a.yaml").as_deref(), Some("a.yaml"));
    assert_eq!(join_relative("records", "../../etc/passwd"), None);
    assert_eq!(join_relative("records", "/etc/passwd"), None);
}
