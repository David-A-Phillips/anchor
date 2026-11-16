//! Ingestion-time enrichment end to end with a scripted System 1 model: tags, summary, the
//! "Answers:" questions chunk, mention edges, skipping of unchanged nodes and failed output.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use anchor::base::{
    ChunkKind, EdgeKind, LanguageModel, Locality, MemoryUsageSink, ModelError, ModelTier, NodeKind,
    SourceKind, TrustLevel,
};
use anchor::config::{AnchorConfig, SourceConfig};
use anchor::engine::Engine;
use anchor::engine::enrich::{ENRICHMENT_SOURCE, EnrichReport};
use anchor::engine::router::{
    ProviderKind, RoutingConfig, ScriptedModel, ScriptedReply, TieredRouter,
};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// One wiki page and two tickets, in that order of node kind (the order enrichment walks them).
fn world(dir: &Path) -> Arc<Engine> {
    write(
        &dir.join("wiki/retries.md"),
        "---\ntitle: Retry runbook\n---\n# Retry runbook\n\nHow the gateway retries failed charges.\n",
    );
    for (key, summary) in [("PAY-1", "Charges fail"), ("PAY-2", "Refunds are slow")] {
        write(
            &dir.join(format!("jira/{key}.json")),
            &serde_json::json!({
                "key": key,
                "fields": { "summary": summary, "description": "Details.", "status": { "name": "Open" } }
            })
            .to_string(),
        );
    }
    let mut config = AnchorConfig {
        data_dir: Some(dir.join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    let mut wiki = SourceConfig::new("wiki", SourceKind::Wiki, dir.join("wiki").to_string_lossy());
    wiki.trust = TrustLevel::ReferenceOnly;
    let mut jira = SourceConfig::new("jira", SourceKind::Jira, dir.join("jira").to_string_lossy());
    jira.trust = TrustLevel::SourceOfTruth;
    config.sources.push(wiki);
    config.sources.push(jira);
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    engine
}

fn router(model: &Arc<ScriptedModel>) -> TieredRouter {
    TieredRouter::new(
        Some(model.clone() as Arc<dyn LanguageModel>),
        None,
        Arc::new(MemoryUsageSink::new()),
        RoutingConfig::default(),
    )
}

fn scripted() -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::new(ModelTier::System1, Locality::Local))
}

const GOOD: &str = r#"Sure: {"summary": "How charges are retried.", "tags": ["Retries", " backoff ", ""],
  "entities": ["PAY-2", "x"], "questions": ["What happens when a charge fails?", "short"]}"#;

fn page(engine: &Engine) -> anchor::engine::store::NodeRecord {
    engine
        .store()
        .nodes_by_kind(NodeKind::WikiPage, 10, 0)
        .unwrap()
        .remove(0)
}

#[tokio::test]
async fn a_node_gets_tags_a_summary_an_answers_chunk_and_mention_edges() {
    let dir = tempfile::tempdir().unwrap();
    let engine = world(dir.path());
    let model = scripted();
    model.always(ScriptedReply::Text(GOOD.to_owned()));

    let report = engine.enrich_with(&router(&model), 10).await.unwrap();
    assert_eq!(
        report,
        EnrichReport {
            enriched: 3,
            unchanged: 0,
            failed: 0,
            remaining: 0
        }
    );
    assert_eq!(model.calls(), 3);

    let page = page(&engine);
    assert_eq!(
        page.attrs.get("summary").and_then(|v| v.as_str()),
        Some("How charges are retried.")
    );
    assert_eq!(
        page.attrs.get("tags").unwrap(),
        &serde_json::json!(["retries", "backoff"]),
        "tags are trimmed, lower-cased and empty ones dropped"
    );
    assert!(page.aliases.iter().any(|alias| alias == "retries"));
    assert!(page.sources.iter().any(|s| s.as_str() == ENRICHMENT_SOURCE));

    let description = engine
        .store()
        .chunks_of(page.id)
        .unwrap()
        .into_iter()
        .find(|chunk| chunk.source.as_str() == ENRICHMENT_SOURCE)
        .expect("a chunk from the enrichment source");
    assert_eq!(description.kind, ChunkKind::Description);
    assert!(
        description
            .text
            .starts_with("Summary: How charges are retried.")
    );
    assert!(description.text.contains("Tags: retries, backoff"));
    assert!(
        description
            .text
            .contains("Answers: What happens when a charge fails?"),
        "{}",
        description.text
    );
    assert!(
        !description.text.contains("short"),
        "questions shorter than 8 characters are dropped"
    );

    // The model-named entity became a low-confidence Mentions edge to the ticket; the 1-character
    // "entity" was ignored.
    let tickets = engine
        .store()
        .nodes_by_kind(NodeKind::Ticket, 10, 0)
        .unwrap();
    let pay2 = tickets
        .iter()
        .find(|t| t.key.as_str().ends_with("PAY-2"))
        .unwrap();
    let edges = engine.store().edges_of(page.id, 100).unwrap();
    let mention = edges
        .iter()
        .find(|e| e.kind == EdgeKind::Mentions && e.to == pay2.id)
        .expect("mention edge to PAY-2");
    assert!((mention.weight - 0.6).abs() < 1e-6, "{}", mention.weight);
    assert_eq!(mention.trust, TrustLevel::ReferenceOnly);
}

#[tokio::test]
async fn unchanged_nodes_are_skipped_and_failures_are_counted_and_retried() {
    let dir = tempfile::tempdir().unwrap();
    let engine = world(dir.path());
    let model = scripted();
    // Walk order: the wiki page first, then the tickets.
    model.push_text(GOOD);
    model.push_text("I cannot help with that");
    model.push_error(ModelError::Unavailable("connection dropped".into()));

    let first = engine.enrich_with(&router(&model), 10).await.unwrap();
    assert_eq!((first.enriched, first.failed, first.unchanged), (1, 2, 0));

    // Second pass: the page is unchanged (no model call), the two failures are tried again.
    model.always(ScriptedReply::Text(GOOD.to_owned()));
    let calls_before = model.calls();
    let second = engine.enrich_with(&router(&model), 10).await.unwrap();
    assert_eq!(
        (second.enriched, second.failed, second.unchanged),
        (2, 0, 1)
    );
    assert_eq!(model.calls() - calls_before, 2);

    // Third pass: nothing left to do.
    let third = engine.enrich_with(&router(&model), 10).await.unwrap();
    assert_eq!((third.enriched, third.unchanged), (0, 3));
    assert_eq!(model.calls() - calls_before, 2, "no further model calls");
}

#[tokio::test]
async fn the_limit_defers_the_rest_and_a_changed_page_is_enriched_again() {
    let dir = tempfile::tempdir().unwrap();
    let engine = world(dir.path());
    let model = scripted();
    model.always(ScriptedReply::Text(GOOD.to_owned()));

    let limited = engine.enrich_with(&router(&model), 1).await.unwrap();
    assert_eq!((limited.enriched, limited.remaining), (1, 2));
    let rest = engine.enrich_with(&router(&model), 10).await.unwrap();
    assert_eq!((rest.enriched, rest.unchanged), (2, 1));

    write(
        &dir.path().join("wiki/retries.md"),
        "---\ntitle: Retry runbook\n---\n# Retry runbook\n\nNow with exponential backoff and jitter.\n",
    );
    engine.scan_all().unwrap();
    let again = engine.enrich_with(&router(&model), 10).await.unwrap();
    assert_eq!((again.enriched, again.unchanged), (1, 2));
}

#[tokio::test]
async fn without_a_system1_model_enrichment_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = world(dir.path());
    // The engine's own router has System 1 disabled.
    assert_eq!(engine.enrich(10).await.unwrap(), EnrichReport::default());
    let bare = TieredRouter::new(
        None,
        None,
        Arc::new(MemoryUsageSink::new()),
        RoutingConfig::default(),
    );
    assert_eq!(
        engine.enrich_with(&bare, 10).await.unwrap(),
        EnrichReport::default()
    );
}
