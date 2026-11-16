//! Behaviours of the query engine that need a purpose-built graph: names shared by several
//! entities, ambiguous ownership, dismissal reasons, documentation quoting and relation chains.

use super::*;
use crate::base::{
    Db, DeviceId, EdgeAssertion, Hlc, MemoryUsageSink, NodeAssertion, SourceDescriptor, SourceKind,
    Unit, UnitKind, UnitVersion, keys,
};
use crate::engine::graph::ContextGraph;
use crate::engine::router::RoutingConfig;

pub(super) const TRUTH: TrustLevel = TrustLevel::SourceOfTruth;
pub(super) const REFERENCE: TrustLevel = TrustLevel::ReferenceOnly;

/// A source to register before the units are applied.
pub(super) fn source(id: &str, kind: SourceKind, trust: TrustLevel) -> SourceDescriptor {
    SourceDescriptor {
        id: SourceId::new(id),
        kind,
        name: id.into(),
        trust,
    }
}

pub(super) fn unit(source: &str, kind: UnitKind, local_id: &str) -> Unit {
    let source = SourceId::new(source);
    Unit::new(keys::unit(&source, kind, local_id), source, kind)
}

/// A query engine over exactly these sources and units.
pub(super) fn world(sources: &[SourceDescriptor], units: Vec<Unit>) -> QueryEngine {
    let db = Db::open_in_memory().expect("db");
    let store = GraphStore::new(db.clone()).expect("store");
    for descriptor in sources {
        store.upsert_source(descriptor).expect("source");
    }
    let version = UnitVersion::new(Hlc::new(1, 0), DeviceId::new("d"));
    store.apply_units(&units, &version).expect("apply");
    let (nodes, edges) = store.load_graph().expect("graph");
    let router = TieredRouter::new(
        None,
        None,
        Arc::new(MemoryUsageSink::new()),
        RoutingConfig::default(),
    );
    QueryEngine::new(QueryDeps {
        store,
        graph: ContextGraph::from_parts(nodes, edges).into_shared(),
        vectors: None,
        embedder: None,
        router: Arc::new(router),
        backlog: Some(Arc::new(Backlog::new(db, &Default::default()))),
        graph_config: GraphConfig::default(),
        query_config: QueryConfig::default(),
    })
}

pub(super) async fn ask(engine: &QueryEngine, question: &str) -> QueryResponse {
    engine
        .query(QueryRequest::new(question))
        .await
        .expect("query")
}

pub(super) fn gap_kinds(response: &QueryResponse) -> Vec<&str> {
    response.gaps.iter().map(|gap| gap.kind.as_str()).collect()
}

/// Two services that each have a file `src/lib.rs`, an endpoint `POST /v1/refunds`, and a runbook
/// thread; each service is owned by its own team.
fn twin_services() -> QueryEngine {
    let mut units = Vec::new();
    for (service, team) in [
        ("billing-api", "Billing Core"),
        ("payments-api", "Payments Core"),
    ] {
        let src = SourceId::new(service);
        let mut code = unit(service, UnitKind::File, "src/lib.rs");
        code.add_node(
            NodeAssertion::new(NodeKind::File, keys::file(&src, "src/lib.rs"), "lib.rs")
                .alias("src/lib.rs")
                .attr(attrs::PATH, "src/lib.rs"),
        );
        code.add_edge(EdgeAssertion::new(
            keys::service(service),
            EdgeKind::Contains,
            keys::file(&src, "src/lib.rs"),
        ));
        units.push(code);

        let mut org = unit("org", UnitKind::OrgModel, service);
        let svc = org.add_node(NodeAssertion::new(
            NodeKind::Service,
            keys::service(service),
            service,
        ));
        let owner = org.add_node(NodeAssertion::new(NodeKind::Team, keys::team(team), team));
        org.add_edge(EdgeAssertion::new(&owner, EdgeKind::Owns, &svc));
        units.push(org);

        let mut portal = unit("portal", UnitKind::ApiSpec, service);
        let api = portal.add_node(NodeAssertion::new(
            NodeKind::ApiSpec,
            keys::api_spec(service),
            service,
        ));
        let endpoint = portal.add_node(
            NodeAssertion::new(
                NodeKind::ApiEndpoint,
                keys::api_endpoint(service, "POST", "/v1/refunds"),
                "POST /v1/refunds",
            )
            .alias("POST /v1/refunds"),
        );
        portal.add_edge(EdgeAssertion::new(&api, EdgeKind::Contains, &endpoint));
        portal.add_edge(EdgeAssertion::new(
            keys::service(service),
            EdgeKind::Implements,
            &api,
        ));
        units.push(portal);
    }
    world(
        &[
            source("billing-api", SourceKind::Repo, TRUTH),
            source("payments-api", SourceKind::Repo, TRUTH),
            source("org", SourceKind::OrgModel, TRUTH),
            source("portal", SourceKind::ApiPortal, TRUTH),
        ],
        units,
    )
}

#[tokio::test]
async fn a_file_name_shared_by_two_services_is_narrowed_by_the_service_the_question_names() {
    let engine = twin_services();
    for (service, owner, other) in [
        ("billing-api", "Billing Core", "Payments Core"),
        ("payments-api", "Payments Core", "Billing Core"),
    ] {
        let response = ask(&engine, &format!("Who owns src/lib.rs in {service}?")).await;
        let answer = response.answer.clone().expect("a direct answer");
        assert!(
            answer.contains(owner) && !answer.contains(other),
            "{answer}"
        );
        assert!(answer.contains("source of truth"), "{answer}");
        assert!(
            !gap_kinds(&response).contains(&"ambiguous_entity"),
            "{:?}",
            response.gaps
        );
        assert_eq!(
            response.context.items[0].node.key,
            format!("team:{}", keys::slugify(owner)),
            "the owner leads the context"
        );
        assert!(
            response.context.items.iter().all(|item| item.node.key
                != format!(
                    "file:{}:src/lib.rs",
                    if service == "billing-api" {
                        "payments-api"
                    } else {
                        "billing-api"
                    }
                )),
            "the other service's file is not offered as evidence"
        );
    }
}

#[tokio::test]
async fn an_endpoint_shared_by_two_services_is_narrowed_the_same_way() {
    let engine = twin_services();
    let response = ask(&engine, "Who owns POST /v1/refunds in payments-api?").await;
    let answer = response.answer.clone().expect("a direct answer");
    assert!(
        answer.contains("Payments Core") && !answer.contains("Billing Core"),
        "{answer}"
    );
    assert!(
        !gap_kinds(&response).contains(&"ambiguous_entity"),
        "{:?}",
        response.gaps
    );
}

#[tokio::test]
async fn an_unscoped_shared_name_is_reported_ambiguous_with_an_answer_per_candidate() {
    let engine = twin_services();
    let response = ask(&engine, "Who owns src/lib.rs?").await;
    assert!(
        gap_kinds(&response).contains(&"ambiguous_entity"),
        "{:?}",
        response.gaps
    );
    let gap = response
        .gaps
        .iter()
        .find(|g| g.kind == "ambiguous_entity")
        .expect("gap");
    assert!(
        gap.detail.contains("2 entities") && gap.detail.contains("src/lib.rs"),
        "{}",
        gap.detail
    );
    let answer = response
        .answer
        .clone()
        .expect("an answer that admits the ambiguity");
    assert!(answer.contains("ambiguous"), "{answer}");
    assert!(
        answer.contains("file:billing-api:src/lib.rs is owned by Billing Core"),
        "{answer}"
    );
    assert!(
        answer.contains("file:payments-api:src/lib.rs is owned by Payments Core"),
        "{answer}"
    );
    assert!(
        answer.contains("Name the service or repository"),
        "{answer}"
    );

    let endpoint = ask(&engine, "Who owns POST /v1/refunds?").await;
    assert!(
        gap_kinds(&endpoint).contains(&"ambiguous_entity"),
        "{:?}",
        endpoint.gaps
    );
    assert!(endpoint.answer.expect("answer").contains("ambiguous"));
}

#[tokio::test]
async fn a_scope_that_matches_no_candidate_does_not_hide_the_ambiguity() {
    let engine = twin_services();
    // `org` is a known source name but no candidate's key has it as a segment: the question is
    // as ambiguous as before, not silently narrowed to nothing.
    let response = ask(&engine, "Who owns src/lib.rs in Billing Core?").await;
    assert!(
        gap_kinds(&response).contains(&"ambiguous_entity"),
        "{:?}",
        response.gaps
    );
}
