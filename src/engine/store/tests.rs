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

// --- graph versions, the delta log, mirrors and consistent reads --------------------------------

fn file_store(dir: &std::path::Path) -> GraphStore {
    let store = GraphStore::new(Db::open(dir.join("anchor.db")).expect("db")).expect("store");
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

fn sorted(graph: &ContextGraph) -> (Vec<GraphNode>, Vec<GraphEdge>) {
    let mut nodes: Vec<_> = graph.nodes().collect();
    let mut edges: Vec<_> = graph.edges().collect();
    nodes.sort_by_key(|n| n.id);
    edges.sort_by_key(|e| e.id);
    (nodes, edges)
}

fn assert_mirrors(store: &GraphStore, graph: &ContextGraph) {
    let image = store.graph_image().expect("image");
    assert_eq!(graph.stamp(), (image.epoch.as_str(), image.version));
    assert_eq!(sorted(graph), (image.nodes, image.edges));
}

/// A unit asserting one service per name, so each call changes the graph.
fn services(names: &[String]) -> Unit {
    let mut unit = unit("repo", &format!("svc-{}", names.join("-")), UnitKind::File);
    for name in names {
        unit.add_node(NodeAssertion::new(
            NodeKind::Service,
            keys::service(name),
            name.clone(),
        ));
    }
    unit
}

#[test]
fn graph_changes_bump_the_version_and_log_their_delta_but_no_ops_do_not() {
    let store = store();
    let (epoch, start) = store.graph_version().expect("version");
    assert_eq!(epoch.len(), 32);
    store.apply_unit(&codeowners(), &version(1)).expect("apply");
    store.apply_unit(&wiki_page(), &version(2)).expect("apply");
    let (_, after) = store.graph_version().expect("version");
    assert_eq!(after, start + 2);

    // Identical content: no graph change, no new version.
    store.apply_unit(&codeowners(), &version(3)).expect("apply");
    assert_eq!(store.graph_version().expect("version").1, after);

    // Replaying the log onto the starting graph reproduces the tables.
    let mut replayed = ContextGraph::new();
    for delta in store
        .deltas_between(0, after)
        .expect("log")
        .expect("complete")
    {
        replayed.apply(&delta);
    }
    let (nodes, edges) = store.load_graph().expect("graph");
    assert_eq!(sorted(&replayed), (nodes, edges));

    // A pruned log cannot serve a replay from before the pruning point.
    store.prune_graph_log(after - 1).expect("prune");
    assert_eq!(store.graph_log_len().expect("len"), 1);
    assert!(store.deltas_between(0, after).expect("log").is_none());
    assert_eq!(
        store
            .deltas_between(after - 1, after)
            .expect("log")
            .map(|d| d.len()),
        Some(1)
    );
}

#[test]
fn a_mirror_follows_every_write_including_removals_and_rebuilds() {
    let store = store();
    let graph = store
        .graph_image()
        .expect("image")
        .into_graph()
        .into_shared();
    let store = store.with_mirror(graph.clone());
    store.apply_unit(&codeowners(), &version(1)).expect("apply");
    store.apply_unit(&wiki_page(), &version(2)).expect("apply");
    assert_mirrors(&store, &graph.read());
    let key = codeowners().key.clone();
    store.remove_unit(&key, &version(3)).expect("remove");
    assert_mirrors(&store, &graph.read());
    store.rebuild().expect("rebuild");
    assert_mirrors(&store, &graph.read());
    // The rebuild cleared the log: readers that were behind must reload.
    assert_eq!(store.graph_log_len().expect("len"), 0);
}

#[test]
fn a_graph_catches_up_with_writes_made_by_another_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reader = file_store(dir.path());
    let graph = reader
        .graph_image()
        .expect("image")
        .into_graph()
        .into_shared();
    let reader = reader.with_mirror(graph.clone());

    // A second handle on the same file, as a daemon in another process would have.
    let writer =
        GraphStore::new(Db::open(dir.path().join("anchor.db")).expect("db")).expect("store");
    writer
        .apply_unit(&codeowners(), &version(1))
        .expect("apply");
    writer.apply_unit(&wiki_page(), &version(2)).expect("apply");
    assert_ne!(graph.read().stamp().1, writer.graph_version().expect("v").1);

    let caught = reader.catch_up().expect("catch up");
    assert_eq!(
        caught,
        CatchUp {
            replayed: 2,
            reloaded: false
        }
    );
    assert_mirrors(&reader, &graph.read());
    assert_eq!(reader.catch_up().expect("again"), CatchUp::default());

    // Once the log no longer reaches back far enough, the graph is reloaded instead.
    writer
        .apply_unit(&services(&["late".into()]), &version(3))
        .expect("apply");
    let (_, now) = writer.graph_version().expect("v");
    writer.prune_graph_log(now).expect("prune");
    let caught = reader.catch_up().expect("catch up");
    assert!(caught.reloaded);
    assert_mirrors(&reader, &graph.read());
}

#[test]
fn consistent_reads_bring_a_stale_mirror_up_to_date_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reader = file_store(dir.path());
    let graph = reader
        .graph_image()
        .expect("image")
        .into_graph()
        .into_shared();
    let reader = reader.with_mirror(graph.clone());
    let writer =
        GraphStore::new(Db::open(dir.path().join("anchor.db")).expect("db")).expect("store");
    writer
        .apply_unit(&codeowners(), &version(1))
        .expect("apply");

    let seen = reader
        .consistent(|graph| {
            let rows: i64 = reader
                .db()
                .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?))?;
            Ok((graph.node_count(), rows as usize))
        })
        .expect("read");
    assert_eq!(seen, (2, 2));
}

#[test]
fn consistent_reads_never_see_the_graph_and_tables_disagree_under_concurrent_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = file_store(dir.path());
    let graph = store
        .graph_image()
        .expect("image")
        .into_graph()
        .into_shared();
    let store = store.with_mirror(graph);

    let writer = {
        let store = store.clone();
        std::thread::spawn(move || {
            for n in 1..=300u64 {
                let names = vec![format!("svc-{n}-a"), format!("svc-{n}-b")];
                store
                    .apply_unit(&services(&names), &version(n))
                    .expect("apply");
            }
        })
    };
    // Check for as long as the writer runs, so every check races a write.
    let mut checks = 0;
    let mut distinct = std::collections::BTreeSet::new();
    while !writer.is_finished() || checks == 0 {
        let (in_memory, in_tables) = store
            .consistent(|graph| {
                let count = || -> Result<i64> {
                    store.db().read(|conn| {
                        Ok(conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?)
                    })
                };
                let before = count()?;
                std::thread::yield_now();
                let after = count()?;
                assert_eq!(before, after, "two reads in one consistent read differ");
                Ok((graph.node_count(), after as usize))
            })
            .expect("consistent");
        assert_eq!(in_memory, in_tables, "graph and tables disagree");
        distinct.insert(in_tables);
        checks += 1;
    }
    writer.join().expect("writer");
    // The reads interleaved with the writes rather than all landing before or after them.
    assert!(
        distinct.len() > 5,
        "only {} distinct states seen",
        distinct.len()
    );
}

#[test]
fn writes_inside_a_consistent_read_are_refused_instead_of_deadlocking() {
    let store = store();
    let graph = store
        .graph_image()
        .expect("image")
        .into_graph()
        .into_shared();
    let store = store.with_mirror(graph);
    let inner = store.clone();
    let result = store.consistent(|_| inner.apply_unit(&codeowners(), &version(1)).map(|_| ()));
    assert!(result.is_err());
    // The store is still usable afterwards.
    store.apply_unit(&codeowners(), &version(2)).expect("apply");
}

#[test]
fn consistent_reads_need_a_mirror() {
    assert!(store().consistent(|_| Ok(())).is_err());
}

#[test]
fn a_local_write_also_brings_in_what_another_process_wrote_before_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let local = file_store(dir.path());
    let graph = local
        .graph_image()
        .expect("image")
        .into_graph()
        .into_shared();
    let local = local.with_mirror(graph.clone());
    let other =
        GraphStore::new(Db::open(dir.path().join("anchor.db")).expect("db")).expect("store");
    other.apply_unit(&codeowners(), &version(1)).expect("apply");
    // The next local write replays the other process's change before its own.
    local.apply_unit(&wiki_page(), &version(2)).expect("apply");
    assert_mirrors(&local, &graph.read());

    // When the log no longer reaches back, the local write rebuilds the mirror instead.
    other
        .apply_unit(&services(&["gap".into()]), &version(3))
        .expect("apply");
    other
        .prune_graph_log(other.graph_version().expect("v").1)
        .expect("prune");
    local
        .apply_unit(&services(&["after-gap".into()]), &version(4))
        .expect("apply");
    assert_mirrors(&local, &graph.read());
}
