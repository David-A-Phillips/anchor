use super::*;
use crate::base::{Chunk, DeviceId, NodeAssertion};
use crate::engine::graph::ContextGraph;

fn store() -> GraphStore {
    let store = GraphStore::new(Db::open_in_memory().expect("db")).expect("store");
    for (id, kind, trust) in [
        ("repo", SourceKind::Repo, TrustLevel::SourceOfTruth),
        ("wiki", SourceKind::Wiki, TrustLevel::ReferenceOnly),
        ("org", SourceKind::OrgModel, TrustLevel::SourceOfTruth),
    ] {
        store
            .upsert_source(&SourceDescriptor {
                id: SourceId::new(id),
                kind,
                name: id.to_owned(),
                trust,
            })
            .expect("source");
    }
    store
}

fn version(n: u64) -> UnitVersion {
    UnitVersion::new(Hlc::new(n, 0), DeviceId::new("dev"))
}

fn unit(source: &str, name: &str, kind: UnitKind) -> Unit {
    let source = SourceId::new(source);
    Unit::new(keys::unit(&source, kind, name), source, kind)
}

fn codeowners() -> Unit {
    let mut unit = unit("repo", "CODEOWNERS", UnitKind::Ownership);
    let team = unit.add_node(NodeAssertion::new(
        NodeKind::Team,
        keys::team("payments"),
        "payments",
    ));
    let service = unit.add_node(NodeAssertion::new(
        NodeKind::Service,
        keys::service("payments-api"),
        "payments-api",
    ));
    unit.add_edge(EdgeAssertion::new(&team, EdgeKind::Owns, &service));
    unit.add_chunk(&service, ChunkKind::Description, "Card payments service");
    unit
}

fn wiki_page() -> Unit {
    let mut unit = unit("wiki", "runbook", UnitKind::WikiPage);
    let page = unit.add_node(
        NodeAssertion::new(
            NodeKind::WikiPage,
            NodeKey::new("wiki:wiki:runbook"),
            "Runbook",
        )
        .alias("Runbook"),
    );
    let team = unit.add_node(NodeAssertion::new(
        NodeKind::Team,
        keys::team("checkout"),
        "Checkout",
    ));
    unit.add_edge(EdgeAssertion::new(
        &team,
        EdgeKind::Owns,
        Lookup::new(&[NodeKind::Service], "Payments API"),
    ));
    unit.add_edge(
        EdgeAssertion::new(
            &page,
            EdgeKind::Mentions,
            Lookup::new(&[NodeKind::Ticket], "PAY-1"),
        )
        .weight(0.9),
    );
    unit.add_chunk(
        &page,
        ChunkKind::Text,
        "The checkout team owns payments-api (stale claim).",
    );
    unit
}

fn mirror(store: &GraphStore) -> ContextGraph {
    let (nodes, edges) = store.load_graph().expect("graph");
    ContextGraph::from_parts(nodes, edges)
}

#[test]
fn conflicting_claims_become_distinct_edges_with_their_trust() {
    let store = store();
    let first = store.apply_unit(&codeowners(), &version(1)).expect("apply");
    assert!(first.changed);
    assert_eq!(first.delta.nodes_upserted.len(), 2);
    assert_eq!(first.delta.edges_upserted.len(), 1);
    assert_eq!(first.chunks_upserted.len(), 1);

    store.apply_unit(&wiki_page(), &version(2)).expect("apply");
    let (nodes, edges) = store.snapshot().expect("snapshot");
    assert_eq!(nodes.len(), 4);
    assert!(edges.contains(&(
        "team:payments".into(),
        "owns".into(),
        "service:payments-api".into(),
        "source_of_truth".into()
    )));
    assert!(edges.contains(&(
        "team:checkout".into(),
        "owns".into(),
        "service:payments-api".into(),
        "reference_only".into()
    )));
    assert_eq!(nodes["team:checkout"].2, "reference_only");
    assert_eq!(nodes["service:payments-api"].2, "source_of_truth");
    assert_eq!(store.pending_edges(10).expect("pending").len(), 1);
}

#[test]
fn identical_content_is_a_no_op_and_units_round_trip() {
    let store = store();
    store.apply_unit(&codeowners(), &version(1)).expect("apply");
    let again = store.apply_unit(&codeowners(), &version(9)).expect("apply");
    assert!(!again.changed && again.delta.is_empty());
    let key = codeowners().key;
    let meta = store.unit_meta(&key).expect("meta").expect("exists");
    assert_eq!(meta.hlc, Hlc::new(1, 0));
    let stored = store.unit(&key).expect("unit").expect("exists");
    assert_eq!(stored.content_hash().expect("hash"), meta.hash);
    assert_eq!(store.max_hlc().expect("hlc"), Hlc::new(1, 0));
}

#[test]
fn pending_edges_resolve_when_the_target_appears_and_unresolve_when_it_goes() {
    let store = store();
    store.apply_unit(&wiki_page(), &version(1)).expect("apply");
    assert_eq!(store.snapshot().expect("snapshot").1.len(), 0);
    assert_eq!(store.pending_edges(10).expect("pending").len(), 2);

    let outcome = store.apply_unit(&codeowners(), &version(2)).expect("apply");
    assert_eq!(outcome.delta.edges_upserted.len(), 2);
    assert_eq!(store.snapshot().expect("snapshot").1.len(), 2);

    let removed = store
        .remove_unit(&codeowners().key, &version(3))
        .expect("remove");
    assert_eq!(removed.delta.nodes_removed.len(), 2);
    assert_eq!(removed.delta.edges_removed.len(), 2);
    assert_eq!(removed.chunks_removed.len(), 1);
    assert_eq!(store.snapshot().expect("snapshot").1.len(), 0);
    assert_eq!(store.pending_edges(10).expect("pending").len(), 2);
    assert!(
        store
            .unit_meta(&codeowners().key)
            .expect("meta")
            .expect("tombstone")
            .deleted
    );
}

#[test]
fn entities_merge_by_alias_and_authority_decides_attributes() {
    let store = store();
    let mut wiki = unit("wiki", "byline", UnitKind::WikiPage);
    wiki.add_node(
        NodeAssertion::new(NodeKind::Person, keys::person("Alice Smith"), "A. Smith")
            .attr("role", "author"),
    );
    store.apply_unit(&wiki, &version(1)).expect("apply");

    let mut org = unit("org", "org.yaml", UnitKind::OrgModel);
    org.add_node(
        NodeAssertion::new(NodeKind::Person, keys::person("asmith"), "Alice Smith")
            .alias("Alice Smith")
            .alias("alice@example.com")
            .attr("role", "Engineering Manager"),
    );
    store.apply_unit(&org, &version(2)).expect("apply");

    let (nodes, _) = store.snapshot().expect("snapshot");
    assert_eq!(nodes.len(), 1, "{nodes:?}");
    let person = store
        .node_by_key(&keys::person("asmith"))
        .expect("node")
        .expect("exists");
    assert_eq!(person.name, "Alice Smith");
    assert_eq!(person.trust, TrustLevel::SourceOfTruth);
    assert_eq!(person.attrs["role"], "Engineering Manager");
    assert_eq!(person.keys.len(), 2);
    assert_eq!(person.sources.len(), 2);
    assert_eq!(
        store
            .resolve(&Lookup::new(&[NodeKind::Person], "alice@example.com"))
            .expect("resolve"),
        Resolution::Resolved(person.id)
    );
}

#[test]
fn scoped_lookups_prefer_the_nearest_candidate_and_ties_are_ambiguous() {
    let store = store();
    let source = SourceId::new("repo");
    for path in ["src/a.rs", "lib/b.rs"] {
        let mut file = unit("repo", path, UnitKind::File);
        file.add_node(
            NodeAssertion::new(
                NodeKind::Symbol,
                keys::symbol(&source, path, "charge"),
                "charge",
            )
            .attr(attrs::PATH, path),
        );
        store.apply_unit(&file, &version(1)).expect("apply");
    }
    let plain = Lookup::new(&[NodeKind::Symbol], "charge");
    assert!(
        matches!(store.resolve(&plain).expect("resolve"), Resolution::Ambiguous(ids) if ids.len() == 2)
    );
    let scoped = plain.clone().in_source("repo").near_path("src/a.rs");
    let Resolution::Resolved(id) = store.resolve(&scoped).expect("resolve") else {
        panic!("scoped lookup must resolve");
    };
    assert_eq!(
        store.node(id).expect("node").expect("exists").attrs[attrs::PATH],
        "src/a.rs"
    );
    assert_eq!(
        store
            .resolve(&Lookup::new(&[NodeKind::Symbol], "missing"))
            .expect("resolve"),
        Resolution::NotFound
    );
}

#[test]
fn trust_change_reweights_and_deltas_keep_a_mirror_in_lockstep() {
    let store = store();
    let mut graph = ContextGraph::new();
    graph.apply(
        &store
            .apply_unit(&codeowners(), &version(1))
            .expect("apply")
            .delta,
    );
    graph.apply(
        &store
            .apply_unit(&wiki_page(), &version(2))
            .expect("apply")
            .delta,
    );
    let delta = store
        .upsert_source(&SourceDescriptor {
            id: SourceId::new("wiki"),
            kind: SourceKind::Wiki,
            name: "wiki".into(),
            trust: TrustLevel::SourceOfTruth,
        })
        .expect("retrust");
    assert!(!delta.is_empty());
    graph.apply(&delta);
    let mut updated = codeowners();
    updated.edges.clear();
    graph.apply(
        &store
            .apply_unit(&updated, &version(3))
            .expect("apply")
            .delta,
    );
    graph.apply(
        &store
            .remove_unit(&wiki_page().key, &version(4))
            .expect("remove")
            .delta,
    );

    let fresh = mirror(&store);
    assert_eq!(graph.node_count(), fresh.node_count());
    assert_eq!(graph.edge_count(), fresh.edge_count());
    for node in fresh.nodes() {
        assert_eq!(graph.node(node.id), Some(node));
    }
    for edge in fresh.edges() {
        assert_eq!(graph.edge(edge.id), Some(edge));
    }
}

#[test]
fn remote_versions_follow_the_supersedes_rule() {
    let store = store();
    let mut unit = codeowners();
    unit.canonicalize();
    let meta = |hlc: u64, deleted: bool, hash: ContentHash| UnitMeta {
        key: unit.key.clone(),
        source: unit.source.clone(),
        kind: unit.kind,
        hash,
        hlc: Hlc::new(hlc, 0),
        origin: DeviceId::new("remote"),
        deleted,
    };
    let hash = unit.content_hash().expect("hash");
    assert!(
        store
            .apply_remote(&meta(5, false, hash), Some(&unit))
            .expect("apply")
            .changed
    );
    assert!(
        !store
            .apply_remote(&meta(4, true, ContentHash::ZERO), None)
            .expect("stale")
            .changed
    );
    assert!(
        store
            .apply_remote(&meta(7, false, ContentHash::of(b"x")), Some(&unit))
            .is_err()
    );
    assert!(
        store
            .apply_remote(&meta(6, true, ContentHash::ZERO), None)
            .expect("tombstone")
            .changed
    );
    assert_eq!(store.snapshot().expect("snapshot").0.len(), 0);
    assert_eq!(
        store
            .unit_metas(&SourceId::new("repo"))
            .expect("metas")
            .len(),
        1
    );
}

fn ledger_file() -> Unit {
    let mut file = unit("repo", "src/lib.rs", UnitKind::File);
    let path = file.add_node(NodeAssertion::new(
        NodeKind::File,
        keys::file(&SourceId::new("repo"), "src/lib.rs"),
        "src/lib.rs",
    ));
    for (name, kind) in [("Ledger", "struct"), ("charge", "function")] {
        let mut node = NodeAssertion::new(
            NodeKind::Symbol,
            crate::base::NodeKey::new(format!("sym:repo:src/lib.rs#{name}")),
            name,
        );
        node.attrs.insert(attrs::SYMBOL_KIND.into(), kind.into());
        node.attrs.insert(attrs::VISIBILITY.into(), "public".into());
        let symbol = file.add_node(node);
        file.add_edge(EdgeAssertion::new(&path, EdgeKind::Contains, &symbol));
    }
    file
}

#[test]
fn apply_remote_view_stores_whole_units_but_materialises_the_view() {
    use crate::config::Depth;
    use crate::engine::depth::view;
    let mut file = ledger_file();
    file.canonicalize();
    let meta = UnitMeta {
        key: file.key.clone(),
        source: file.source.clone(),
        kind: file.kind,
        hash: file.content_hash().expect("hash"),
        hlc: Hlc::new(5, 0),
        origin: DeviceId::new("remote"),
        deleted: false,
    };

    let plain = store();
    plain.apply_remote(&meta, Some(&file)).expect("plain");

    let thin = store();
    thin.apply_remote_view(&meta, Some(&file), &view(Depth::Map))
        .expect("view");
    assert_eq!(thin.unit(&file.key).expect("unit"), Some(file.clone()));
    let stored = thin.unit_meta(&file.key).expect("meta").expect("meta");
    assert_eq!(stored.hash, meta.hash);
    let keys: Vec<String> = thin.snapshot().expect("snap").0.into_keys().collect();
    assert!(keys.iter().any(|k| k.ends_with("#Ledger")), "{keys:?}");
    assert!(!keys.iter().any(|k| k.ends_with("#charge")), "{keys:?}");

    // Nothing materialised at all, yet the unit is stored.
    let none = store();
    none.apply_remote_view(&meta, Some(&file), &|_| None)
        .expect("none");
    assert!(none.snapshot().expect("snap").0.is_empty());
    assert!(none.unit(&file.key).expect("unit").is_some());

    // Rematerialising to full yields exactly what a plain apply gives.
    let repo = SourceId::new("repo");
    let outcome = thin
        .rematerialize_source(&repo, &view(Depth::Full))
        .expect("rematerialize");
    assert!(outcome.changed);
    assert_eq!(
        thin.snapshot().expect("snap"),
        plain.snapshot().expect("snap")
    );
    none.rematerialize_source(&repo, &view(Depth::Full))
        .expect("rematerialize");
    assert_eq!(
        none.snapshot().expect("snap"),
        plain.snapshot().expect("snap")
    );
    // And back down again.
    thin.rematerialize_source(&repo, &view(Depth::Map))
        .expect("down");
    let keys: Vec<String> = thin.snapshot().expect("snap").0.into_keys().collect();
    assert!(!keys.iter().any(|k| k.ends_with("#charge")), "{keys:?}");
}

#[test]
fn text_search_and_chunk_ids_are_stable() {
    let store = store();
    let first = store.apply_unit(&codeowners(), &version(1)).expect("apply");
    let mut changed = codeowners();
    changed.chunks.push(Chunk::new(
        keys::service("payments-api"),
        1,
        ChunkKind::Description,
        "Handles refunds",
    ));
    let second = store.apply_unit(&changed, &version(2)).expect("apply");
    assert_eq!(second.chunks_upserted.len(), 1);
    assert!(second.chunks_removed.is_empty());
    assert_ne!(first.chunks_upserted[0], second.chunks_upserted[0]);

    let hits = store
        .search_chunks("refunds AND \"oops", 5)
        .expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].chunk, second.chunks_upserted[0]);
    assert!(store.search_chunks("  ", 5).expect("search").is_empty());
    let nodes = store
        .search_nodes("Payments API", &[NodeKind::Service], 5)
        .expect("search");
    assert_eq!(nodes[0].score, 1.0);
    assert_eq!(store.search_nodes("pay", &[], 5).expect("search").len(), 2);
    assert_eq!(store.chunks_after(ChunkId(0), 10).expect("chunks").len(), 2);
    let stats = store.stats().expect("stats");
    assert_eq!(
        (stats.node_count, stats.edge_count, stats.chunk_count),
        (2, 1, 2)
    );
}

#[test]
fn rebuild_reproduces_the_graph_and_unknown_sources_are_rejected() {
    let store = store();
    store.apply_unit(&wiki_page(), &version(1)).expect("apply");
    store.apply_unit(&codeowners(), &version(2)).expect("apply");
    let before = store.snapshot().expect("snapshot");
    store.rebuild().expect("rebuild");
    assert_eq!(store.snapshot().expect("snapshot"), before);
    assert!(
        store
            .apply_unit(&unit("nope", "x", UnitKind::File), &version(3))
            .is_err()
    );
}

#[test]
fn small_unit_update_is_fast() {
    let store = store();
    let source = SourceId::new("repo");
    let build = |extra: usize| {
        let mut file = unit("repo", "src/big.rs", UnitKind::File);
        let file_key = file.add_node(NodeAssertion::new(
            NodeKind::File,
            keys::file(&source, "src/big.rs"),
            "big.rs",
        ));
        for i in 0..30 + extra {
            let symbol = file.add_node(NodeAssertion::new(
                NodeKind::Symbol,
                keys::symbol(&source, "src/big.rs", &format!("f{i}")),
                format!("f{i}"),
            ));
            file.add_edge(EdgeAssertion::new(&file_key, EdgeKind::Contains, &symbol));
            for call in 0..3 {
                file.add_edge(EdgeAssertion::new(
                    &symbol,
                    EdgeKind::Calls,
                    Lookup::new(&[NodeKind::Symbol], format!("f{}", (i + call + 1) % 30)),
                ));
            }
        }
        file
    };
    store.apply_unit(&build(0), &version(1)).expect("apply");
    let started = std::time::Instant::now();
    store.apply_unit(&build(1), &version(2)).expect("apply");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(100),
        "{:?}",
        started.elapsed()
    );
}
