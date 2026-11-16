//! Ingestion-time enrichment: the local model summarises and tags text-bearing nodes.
//!
//! This is where models improve the map itself. It runs in the background after ingestion, never
//! while someone waits for an answer. For each wiki page, ticket, pull request and API it asks
//! System 1 for a one-sentence summary, a few topic tags and the entities the text is about, and
//! records them as a unit of the built-in `anchor-enrichment` source:
//!
//! * `summary` and `tags` attributes on the node, and the tags as aliases, so a search for a
//!   topic finds the page;
//! * a description chunk (summary and tags) for full-text and vector search;
//! * `Mentions` edges to the entities the model named, resolved by the linker like any other.
//!
//! The source is `reference_only`: a model's opinion never overrides an authoritative fact.
//! Nodes whose text has not changed since they were enriched are skipped.

use serde::{Deserialize, Serialize};

use super::core::Engine;
use super::router::TieredRouter;
use crate::base::{
    CallPurpose, ChunkKind, CompletionRequest, ContentHash, EdgeAssertion, EdgeKind, Lookup,
    ModelTier, NodeAssertion, NodeKind, ResponseFormat, Result, SourceDescriptor, SourceId,
    SourceKind, TrustLevel, Unit, UnitKind, keys,
};

/// Id of the built-in source that holds model-derived facts.
pub const ENRICHMENT_SOURCE: &str = "anchor-enrichment";

const KINDS: &[NodeKind] = &[
    NodeKind::WikiPage,
    NodeKind::Ticket,
    NodeKind::PullRequest,
    NodeKind::ApiSpec,
];
const MAX_TEXT: usize = 3000;

/// What one enrichment pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EnrichReport {
    /// Nodes summarised and tagged in this pass.
    pub enriched: usize,
    /// Nodes skipped because their text is unchanged since they were enriched.
    pub unchanged: usize,
    /// Nodes the model failed on (unusable output or a failed call).
    pub failed: usize,
    /// Nodes left for a later pass because the limit was reached.
    pub remaining: usize,
}

#[derive(Debug, Default, Deserialize)]
struct ModelOutput {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
    /// Questions the text answers, phrased the way someone who has not read it would ask.
    #[serde(default)]
    questions: Vec<String>,
}

fn parse(text: &str) -> Option<ModelOutput> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let output: ModelOutput = serde_json::from_str(text.get(start..=end)?).ok()?;
    (!output.summary.trim().is_empty() || !output.tags.is_empty()).then_some(output)
}

fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

impl Engine {
    /// Summarises and tags up to `limit` nodes with the System 1 model. Does nothing when no
    /// System 1 model is configured.
    pub async fn enrich(&self, limit: usize) -> Result<EnrichReport> {
        self.enrich_with(&self.router(), limit).await
    }

    /// [`Self::enrich`] against an explicit router (the engine's own one in production; a
    /// scripted one in tests).
    pub async fn enrich_with(&self, router: &TieredRouter, limit: usize) -> Result<EnrichReport> {
        let mut report = EnrichReport::default();
        if router.model_info(ModelTier::System1).is_none() {
            return Ok(report);
        }
        let source = SourceId::new(ENRICHMENT_SOURCE);
        let store = self.store().clone();
        let registered = source.clone();
        // (node key, kind, name, text, hash of the text)
        let candidates = tokio::task::spawn_blocking(move || -> Result<Vec<_>> {
            store.upsert_source(&SourceDescriptor {
                id: registered.clone(),
                kind: SourceKind::Derived,
                name: "Model-derived summaries and tags".to_owned(),
                trust: TrustLevel::ReferenceOnly,
            })?;
            let mut out = Vec::new();
            for kind in KINDS {
                for node in store.nodes_by_kind(*kind, 100_000, 0)? {
                    let mut text = node.name.clone();
                    for chunk in store.chunks_of(node.id)? {
                        // Skip our own summaries so enrichment does not feed on itself.
                        if chunk.source == registered {
                            continue;
                        }
                        text.push_str("\n\n");
                        text.push_str(&chunk.text);
                        if text.len() >= MAX_TEXT {
                            break;
                        }
                    }
                    let text = clip(&text, MAX_TEXT).to_owned();
                    let hash = ContentHash::of(text.as_bytes()).to_hex();
                    let unit_key = keys::unit(&registered, UnitKind::Enrichment, node.key.as_str());
                    let current = store.unit(&unit_key)?.and_then(|unit| {
                        unit.nodes.first().and_then(|n| {
                            n.attrs
                                .get("enriched_from")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned)
                        })
                    });
                    out.push((
                        node.key,
                        node.kind,
                        node.name,
                        text,
                        hash.clone(),
                        current == Some(hash),
                    ));
                }
            }
            Ok(out)
        })
        .await??;

        let mut units = Vec::new();
        for (key, kind, name, text, hash, unchanged) in candidates {
            if unchanged {
                report.unchanged += 1;
                continue;
            }
            if report.enriched + report.failed >= limit {
                report.remaining += 1;
                continue;
            }
            let prompt = format!(
                "Summarise this {kind} for an engineering knowledge graph. Reply with JSON only: {{\"summary\": one sentence, \"tags\": 3 to 6 short lowercase topic tags, \"entities\": names of the services, APIs, teams or tickets it is about, exactly as written, \"questions\": 2 or 3 short questions this text answers, worded as someone who has not read it would ask, using everyday synonyms rather than the text's own terms}}.\n\n{text}"
            );
            let request = CompletionRequest::prompt(CallPurpose::Other, prompt)
                .with_format(ResponseFormat::Json)
                .with_max_tokens(400);
            let output = match router.complete(ModelTier::System1, request).await {
                Ok(routed) => parse(&routed.completion.text),
                Err(err) => {
                    tracing::debug!("enriching {key} failed: {err}");
                    None
                }
            };
            let Some(output) = output else {
                report.failed += 1;
                continue;
            };
            let tags: Vec<String> = output
                .tags
                .iter()
                .map(|tag| tag.trim().to_lowercase())
                .filter(|tag| !tag.is_empty() && tag.len() <= 40)
                .take(8)
                .collect();
            let mut unit = Unit::new(
                keys::unit(&source, UnitKind::Enrichment, key.as_str()),
                source.clone(),
                UnitKind::Enrichment,
            );
            let mut node = NodeAssertion::new(kind, key.clone(), name)
                .attr("summary", output.summary.trim())
                .attr("tags", tags.clone())
                .attr("enriched_from", hash);
            for tag in &tags {
                node = node.alias(tag.as_str());
            }
            let node = unit.add_node(node);
            // The questions are what lets a differently worded query find the text by
            // full-text search, without a neural embedding model.
            let questions: Vec<&str> = output
                .questions
                .iter()
                .map(|q| q.trim())
                .filter(|q| q.len() >= 8 && q.len() <= 200)
                .take(4)
                .collect();
            let mut description = format!(
                "Summary: {}\nTags: {}",
                output.summary.trim(),
                tags.join(", ")
            );
            if !questions.is_empty() {
                description.push_str(&format!("\nAnswers: {}", questions.join(" | ")));
            }
            unit.add_chunk(&node, ChunkKind::Description, description);
            for entity in output.entities.iter().take(8) {
                let entity = entity.trim();
                if entity.len() >= 3 {
                    unit.add_edge(
                        EdgeAssertion::new(
                            &node,
                            EdgeKind::Mentions,
                            Lookup::new(
                                &[
                                    NodeKind::Service,
                                    NodeKind::ApiSpec,
                                    NodeKind::Component,
                                    NodeKind::Team,
                                    NodeKind::Ticket,
                                ],
                                entity,
                            ),
                        )
                        .weight(0.6),
                    );
                }
            }
            units.push(unit.finish());
            report.enriched += 1;
        }
        if !units.is_empty() {
            let engine_units = units;
            // `apply` is blocking; the engine is used through `&self`, so run it in place on a
            // blocking-capable thread.
            // `block_in_place` is only allowed on the multi-threaded runtime; on a current-thread
            // one (some embedders, `#[tokio::test]`) applying inline is the only option.
            if matches!(
                tokio::runtime::Handle::current().runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            ) {
                tokio::task::block_in_place(|| self.apply(&engine_units))?;
            } else {
                self.apply(&engine_units)?;
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_output_is_parsed_leniently() {
        let output = parse("Sure!\n```json\n{\"summary\": \"Runbook for retries.\", \"tags\": [\"Retries\", \"ops\"], \"entities\": [\"payments-api\"]}\n```").expect("parsed");
        assert_eq!(output.summary, "Runbook for retries.");
        assert_eq!(output.tags, vec!["Retries", "ops"]);
        assert_eq!(output.entities, vec!["payments-api"]);
        assert!(parse("no json here").is_none());
        assert!(parse("{\"summary\": \"\", \"tags\": []}").is_none());
        assert_eq!(clip("héllo", 2), "h");
    }
}
