//! The running engine: opens every component, ingests sources into the unified graph and keeps
//! the in-memory graph in lockstep with the store.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::broadcast;

use super::embed::{HashEmbedder, VectorIndex, build_embedder, index_chunks};
use super::graph::{ContextGraph, SharedGraph};
use super::ingest::ownership::{
    OwnershipFileKind, OwnershipRules, ownership_file_kind, parse_codeowners, parse_maintainers,
    parse_owners_tree,
};
use super::ingest::{IngestOutput, api_portal, docs, jira, org, review, wiki};
use super::parser::{IncrementalParser, Language, build_file_unit};
use super::router::TieredRouter;
use super::store::{ApplyOutcome, GraphStore};
use crate::backlog::Backlog;
use crate::base::{
    ChunkId, DeviceId, EdgeAssertion, EdgeKind, Embedder, Error, HlcClock, NodeAssertion, NodeKind,
    Result, SourceId, SourceKind, Unit, UnitKey, UnitKind, UnitVersion, attrs, keys,
};
use crate::config::{AnchorConfig, AnchorPaths, SourceConfig};
use crate::metrics::{PriceTable, TokenLedger};
use crate::sync::identity::DeviceIdentity;

/// Something that happened inside the engine; published on [`Engine::subscribe`].
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    /// The graph changed.
    GraphChanged {
        /// Nodes created or updated.
        nodes_upserted: usize,
        /// Nodes removed.
        nodes_removed: usize,
        /// Edges created or updated.
        edges_upserted: usize,
        /// Edges removed.
        edges_removed: usize,
    },
    /// A changed file was ingested by the watcher path.
    FileIngested {
        /// Source the file belongs to.
        source: String,
        /// Path relative to the source root.
        path: String,
        /// Microseconds from detection to the graph being updated.
        latency_us: u64,
    },
    /// A source scan finished.
    ScanFinished {
        /// The report.
        report: ScanReport,
    },
    /// A non-fatal problem.
    Warning {
        /// What happened.
        message: String,
    },
}

/// Result of scanning one source.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ScanReport {
    /// Source scanned.
    pub source: String,
    /// Files or documents seen.
    pub files_seen: usize,
    /// Files skipped because they were unchanged since the last scan.
    pub files_unchanged: usize,
    /// Units produced and applied (identical content is not rewritten).
    pub units_applied: usize,
    /// Units removed because their file or document disappeared.
    pub units_removed: usize,
    /// Nodes created or updated.
    pub nodes_upserted: usize,
    /// Edges created or updated.
    pub edges_upserted: usize,
    /// Ingestion warnings.
    pub warnings: Vec<String>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Percentiles of the change-to-graph latency, in microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LatencyStats {
    /// Samples recorded.
    pub count: usize,
    /// Median.
    pub p50_us: u64,
    /// 95th percentile.
    pub p95_us: u64,
    /// Maximum.
    pub max_us: u64,
}

const LATENCY_WINDOW: usize = 1024;
const BATCH: usize = 256;

/// The assembled engine. Create with [`Engine::open`]; share as `Arc<Engine>`.
pub struct Engine {
    config: AnchorConfig,
    paths: AnchorPaths,
    store: GraphStore,
    graph: SharedGraph,
    clock: HlcClock,
    device: DeviceId,
    identity: DeviceIdentity,
    parser: IncrementalParser,
    embedder: Arc<dyn Embedder>,
    vectors: Arc<VectorIndex>,
    router: Arc<TieredRouter>,
    ledger: Arc<TokenLedger>,
    backlog: Arc<Backlog>,
    events: broadcast::Sender<EngineEvent>,
    pending_chunks: Mutex<Vec<ChunkId>>,
    latencies: Mutex<Vec<u64>>,
    ownership: Mutex<HashMap<SourceId, Arc<OwnershipRules>>>,
    manifest: Mutex<HashMap<(SourceId, String), (u64, i128)>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("data_dir", &self.paths.root())
            .finish()
    }
}

fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

struct LedgerSpend(Arc<TokenLedger>);

impl super::router::SpendTracker for LedgerSpend {
    fn cloud_spend_today_usd(&self) -> f64 {
        self.0.cloud_spend_today_usd().unwrap_or(0.0)
    }
}

impl Engine {
    /// Opens (creating if needed) the engine for `config`. Blocking: call from a blocking thread.
    pub fn open(config: AnchorConfig) -> Result<Arc<Self>> {
        let paths = config.paths();
        paths.ensure()?;
        let db = crate::base::Db::open(paths.db())?;
        let store = GraphStore::new(db.clone())?;
        for source in config.sources.iter().filter(|source| source.enabled) {
            store.upsert_source(&source.descriptor())?;
        }
        let (nodes, edges) = store.load_graph()?;
        let graph = ContextGraph::from_parts(nodes, edges).into_shared();
        let clock = HlcClock::starting_after(store.max_hlc()?);
        let identity = DeviceIdentity::load_or_create(&paths.keys_dir())?;
        let device = identity.device_id().clone();

        let fallback = || -> Result<Arc<dyn Embedder>> {
            Ok(Arc::new(HashEmbedder::new(
                config.embeddings.dimensions.max(16),
            )?))
        };
        let embedder = match build_embedder(&config.embeddings, &paths, &config.ollama) {
            Ok(embedder) if embedder.dimensions() > 0 => embedder,
            Ok(_) => fallback()?,
            Err(err) => {
                tracing::warn!("embedding backend unavailable ({err}); using the hash embedder");
                fallback()?
            }
        };
        let vectors = Arc::new(VectorIndex::load(
            &db,
            embedder.model_id(),
            embedder.dimensions(),
        )?);

        let ledger = Arc::new(TokenLedger::new(
            db.clone(),
            PriceTable::new(&config.metrics),
        )?);
        let router = TieredRouter::from_config(&config.models, ledger.clone(), &process_env)?
            .with_spend_tracker(Arc::new(LedgerSpend(ledger.clone())));
        let backlog = Arc::new(Backlog::new(db.clone(), &config.backlog));
        let (events, _) = broadcast::channel(1024);

        Ok(Arc::new(Self {
            parser: IncrementalParser::new(&config.parser),
            config,
            paths,
            store,
            graph,
            clock,
            device,
            identity,
            embedder,
            vectors,
            router: Arc::new(router),
            ledger,
            backlog,
            events,
            pending_chunks: Mutex::new(Vec::new()),
            latencies: Mutex::new(Vec::new()),
            ownership: Mutex::new(HashMap::new()),
            manifest: Mutex::new(HashMap::new()),
        }))
    }

    /// The configuration the engine was opened with.
    pub fn config(&self) -> &AnchorConfig {
        &self.config
    }
    /// Filesystem layout.
    pub fn paths(&self) -> &AnchorPaths {
        &self.paths
    }
    /// The graph store.
    pub fn store(&self) -> &GraphStore {
        &self.store
    }
    /// The shared in-memory graph.
    pub fn graph(&self) -> SharedGraph {
        self.graph.clone()
    }
    /// This device's id (origin of local unit versions).
    pub fn device_id(&self) -> &DeviceId {
        &self.device
    }
    /// The embedder in use.
    pub fn embedder(&self) -> Arc<dyn Embedder> {
        self.embedder.clone()
    }
    /// The vector index.
    pub fn vectors(&self) -> Arc<VectorIndex> {
        self.vectors.clone()
    }
    /// The tiered model router.
    pub fn router(&self) -> Arc<TieredRouter> {
        self.router.clone()
    }
    /// The token ledger.
    pub fn ledger(&self) -> Arc<TokenLedger> {
        self.ledger.clone()
    }
    /// The gap-analysis backlog.
    pub fn backlog(&self) -> Arc<Backlog> {
        self.backlog.clone()
    }
    /// Subscribes to engine events.
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    fn version(&self) -> UnitVersion {
        UnitVersion::new(self.clock.now(), self.device.clone())
    }

    fn publish(&self, event: EngineEvent) {
        // No subscriber is not an error.
        let _ = self.events.send(event);
    }

    /// Keeps the in-memory graph, vector index and listeners in lockstep with a store mutation.
    /// Used for local writes and by the sync layer after applying remote units.
    pub fn absorb(&self, outcome: &ApplyOutcome) -> Result<()> {
        if !outcome.delta.is_empty() {
            self.graph.write().apply(&outcome.delta);
            self.publish(EngineEvent::GraphChanged {
                nodes_upserted: outcome.delta.nodes_upserted.len(),
                nodes_removed: outcome.delta.nodes_removed.len(),
                edges_upserted: outcome.delta.edges_upserted.len(),
                edges_removed: outcome.delta.edges_removed.len(),
            });
        }
        if !outcome.chunks_removed.is_empty() {
            self.vectors.remove(&outcome.chunks_removed)?;
        }
        if !outcome.chunks_upserted.is_empty() {
            self.pending_chunks.lock().extend(&outcome.chunks_upserted);
        }
        Ok(())
    }

    /// The write path for locally produced units.
    pub fn apply(&self, units: &[Unit]) -> Result<ApplyOutcome> {
        let outcome = self.store.apply_units(units, &self.version())?;
        self.absorb(&outcome)?;
        Ok(outcome)
    }

    /// Removes units (their file or document disappeared).
    pub fn remove(&self, keys: &[UnitKey]) -> Result<ApplyOutcome> {
        let mut total = ApplyOutcome::default();
        for key in keys {
            let outcome = self.store.remove_unit(key, &self.version())?;
            self.absorb(&outcome)?;
            total.absorb(outcome);
        }
        Ok(total)
    }

    /// Embeds chunks created since the last call. Run it off the hot path (after scans, or from
    /// a background task); returns the number of chunks embedded.
    pub async fn embed_pending(&self) -> Result<usize> {
        let ids: Vec<ChunkId> = std::mem::take(&mut *self.pending_chunks.lock());
        if ids.is_empty() {
            return Ok(0);
        }
        let store = self.store.clone();
        let chunks = tokio::task::spawn_blocking(move || -> Result<Vec<_>> {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(chunk) = store.chunk(id)? {
                    out.push((chunk.id, chunk.hash, chunk.text));
                }
            }
            Ok(out)
        })
        .await??;
        let report = index_chunks(
            self.embedder.as_ref(),
            &self.vectors,
            chunks,
            self.config.embeddings.batch_size.max(1),
        )
        .await?;
        Ok(report.embedded)
    }

    /// Change-to-graph latency percentiles over the most recent file changes.
    pub fn latency_stats(&self) -> LatencyStats {
        let mut samples = self.latencies.lock().clone();
        if samples.is_empty() {
            return LatencyStats::default();
        }
        samples.sort_unstable();
        let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
        LatencyStats {
            count: samples.len(),
            p50_us: at(0.5),
            p95_us: at(0.95),
            max_us: samples[samples.len() - 1],
        }
    }

    fn source(&self, id: &SourceId) -> Result<&SourceConfig> {
        self.config
            .source(id)
            .ok_or_else(|| Error::not_found(format!("source `{id}` is not configured")))
    }

    /// Scans every enabled source: directories and definitions first, discussions last.
    pub fn scan_all(&self) -> Result<Vec<ScanReport>> {
        let order = |kind: SourceKind| match kind {
            SourceKind::OrgModel => 0,
            SourceKind::Repo => 1,
            SourceKind::ApiPortal => 2,
            SourceKind::Wiki => 3,
            SourceKind::Jira => 4,
            SourceKind::CodeReview => 5,
        };
        let mut sources: Vec<&SourceConfig> =
            self.config.sources.iter().filter(|s| s.enabled).collect();
        sources.sort_by_key(|source| (order(source.kind), source.id.clone()));
        sources
            .into_iter()
            .map(|source| self.scan_source(&source.id))
            .collect()
    }

    /// Scans one source and brings the graph up to date with it.
    pub fn scan_source(&self, id: &SourceId) -> Result<ScanReport> {
        let started = Instant::now();
        let source = self.source(id)?;
        let root = PathBuf::from(&source.location);
        let mut report = ScanReport {
            source: id.to_string(),
            ..ScanReport::default()
        };
        let mut produced: BTreeSet<UnitKey> = BTreeSet::new();
        let mut complete = true;
        match source.kind {
            SourceKind::Repo => {
                complete = self.scan_repo(source, &root, &mut report, &mut produced)?
            }
            SourceKind::Wiki => self.apply_output(
                wiki::scan_wiki(&root, id, &Default::default())?,
                &mut report,
                &mut produced,
            )?,
            SourceKind::Jira => self.apply_output(
                jira::scan_jira(&root, id, &Default::default())?,
                &mut report,
                &mut produced,
            )?,
            SourceKind::ApiPortal => self.apply_output(
                api_portal::scan_api_portal(&root, id, &Default::default())?,
                &mut report,
                &mut produced,
            )?,
            SourceKind::CodeReview => self.apply_output(
                review::scan_reviews(&root, id, &Default::default())?,
                &mut report,
                &mut produced,
            )?,
            SourceKind::OrgModel => {
                let files: Vec<PathBuf> = if root.is_dir() {
                    let mut files: Vec<PathBuf> = std::fs::read_dir(&root)?
                        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                        .filter(|path| path.is_file())
                        .collect();
                    files.sort();
                    files
                } else {
                    vec![root.clone()]
                };
                for file in files {
                    let name = file
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    report.files_seen += 1;
                    match std::fs::read_to_string(&file)
                        .map_err(Error::from)
                        .and_then(|text| org::parse_org_model(id, &name, &text))
                    {
                        Ok(output) => self.apply_output(output, &mut report, &mut produced)?,
                        Err(err) => report.warnings.push(format!("{name}: {err}")),
                    }
                }
            }
        }
        if complete {
            let stale: Vec<UnitKey> = self
                .store
                .unit_metas(id)?
                .into_iter()
                .filter(|meta| !meta.deleted && !produced.contains(&meta.key))
                .map(|meta| meta.key)
                .collect();
            report.units_removed = stale.len();
            self.remove(&stale)?;
        }
        report.duration_ms = started.elapsed().as_millis() as u64;
        self.publish(EngineEvent::ScanFinished {
            report: report.clone(),
        });
        Ok(report)
    }

    fn apply_batch(&self, units: &mut Vec<Unit>, report: &mut ScanReport) -> Result<()> {
        if units.is_empty() {
            return Ok(());
        }
        let outcome = self.apply(units)?;
        report.units_applied += units.len();
        report.nodes_upserted += outcome.delta.nodes_upserted.len();
        report.edges_upserted += outcome.delta.edges_upserted.len();
        units.clear();
        Ok(())
    }

    fn apply_output(
        &self,
        output: IngestOutput,
        report: &mut ScanReport,
        produced: &mut BTreeSet<UnitKey>,
    ) -> Result<()> {
        report.files_seen += output.units.len();
        report
            .warnings
            .extend(output.warnings.iter().map(ToString::to_string));
        let mut batch = Vec::with_capacity(BATCH);
        for unit in output.units {
            produced.insert(unit.key.clone());
            batch.push(unit);
            if batch.len() >= BATCH {
                self.apply_batch(&mut batch, report)?;
            }
        }
        self.apply_batch(&mut batch, report)
    }

    fn walk(&self, source: &SourceConfig, root: &Path) -> Result<Vec<(PathBuf, String)>> {
        let mut overrides = ignore::overrides::OverrideBuilder::new(root);
        for pattern in &source.include {
            overrides
                .add(pattern)
                .map_err(|err| Error::config(err.to_string()))?;
        }
        for pattern in &source.exclude {
            overrides
                .add(&format!("!{pattern}"))
                .map_err(|err| Error::config(err.to_string()))?;
        }
        let overrides = overrides
            .build()
            .map_err(|err| Error::config(err.to_string()))?;
        let mut files = Vec::new();
        for entry in ignore::WalkBuilder::new(root)
            .hidden(false)
            .overrides(overrides)
            .build()
        {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else {
                continue;
            };
            let rel = keys::normalize_path(&rel.to_string_lossy());
            if rel.starts_with(".git/") {
                continue;
            }
            files.push((entry.into_path(), rel));
        }
        files.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(files)
    }

    fn load_ownership(
        &self,
        id: &SourceId,
        root: &Path,
        files: &[(PathBuf, String)],
        report: &mut ScanReport,
    ) -> (Option<Arc<OwnershipRules>>, Vec<Unit>) {
        let mut units = Vec::new();
        let mut rules = None;
        let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
        let kinds: Vec<(&(PathBuf, String), OwnershipFileKind)> = files
            .iter()
            .filter_map(|file| ownership_file_kind(&file.1).map(|kind| (file, kind)))
            .collect();
        let pick = |wanted: OwnershipFileKind| {
            kinds
                .iter()
                .find(|(_, kind)| *kind == wanted)
                .map(|(file, _)| *file)
        };
        let parsed = if let Some((path, rel)) = pick(OwnershipFileKind::Codeowners) {
            Some((rel.clone(), parse_codeowners(&read(path))))
        } else if let Some((path, rel)) = pick(OwnershipFileKind::Maintainers) {
            Some((rel.clone(), parse_maintainers(&read(path))))
        } else if kinds
            .iter()
            .any(|(_, kind)| *kind == OwnershipFileKind::Owners)
        {
            let texts: Vec<(String, String)> = kinds
                .iter()
                .filter(|(_, kind)| *kind == OwnershipFileKind::Owners)
                .map(|((path, rel), _)| (rel.clone(), read(path)))
                .collect();
            let aliases = pick(OwnershipFileKind::OwnersAliases).map(|(path, _)| read(path));
            Some((
                "OWNERS".to_owned(),
                parse_owners_tree(
                    texts
                        .iter()
                        .map(|(rel, text)| (rel.as_str(), text.as_str())),
                    aliases.as_deref(),
                ),
            ))
        } else {
            None
        };
        let _ = root;
        if let Some((rel, result)) = parsed {
            match result {
                Ok(parsed) => {
                    report
                        .warnings
                        .extend(parsed.warnings().iter().map(ToString::to_string));
                    units.push(parsed.definitions_unit(id, &rel));
                    rules = Some(Arc::new(parsed));
                }
                Err(err) => report.warnings.push(format!("{rel}: {err}")),
            }
        }
        match &rules {
            Some(rules) => self.ownership.lock().insert(id.clone(), rules.clone()),
            None => self.ownership.lock().remove(id),
        };
        (rules, units)
    }

    /// Builds the unit of one repository file, or `None` when the file is not ingested.
    fn file_unit(
        &self,
        id: &SourceId,
        rel: &str,
        text: &str,
        rules: Option<&OwnershipRules>,
    ) -> Result<Option<Unit>> {
        let lower = rel.to_ascii_lowercase();
        let name = lower.rsplit('/').next().unwrap_or(&lower);
        let mut unit = if let Some(language) = Language::from_path(rel) {
            let outcome = self.parser.parse(&format!("{id}:{rel}"), language, text)?;
            build_file_unit(id, rel, text, &outcome.parsed)
        } else if docs::doc_language(rel).is_some() {
            docs::parse_doc_file(id, rel, text)
        } else if (name.contains("openapi") || name.contains("swagger"))
            && (name.ends_with(".yaml") || name.ends_with(".yml") || name.ends_with(".json"))
        {
            let mut unit = api_portal::parse_openapi(id, id.as_str(), text)?;
            let api = unit
                .nodes
                .iter()
                .find(|node| node.kind == NodeKind::ApiSpec)
                .map(|node| node.key.clone());
            if let Some(api) = api {
                unit.add_edge(EdgeAssertion::new(
                    keys::service(id.as_str()),
                    EdgeKind::Implements,
                    api,
                ));
            }
            unit.key = keys::unit(id, UnitKind::File, rel);
            unit.kind = UnitKind::File;
            unit
        } else {
            return Ok(None);
        };
        if let Some(rules) = rules {
            let file_key = keys::file(id, rel);
            if unit.nodes.iter().any(|node| node.key == file_key) {
                unit.nodes.extend(rules.file_parties(rel));
                unit.edges.extend(rules.file_edges(&file_key, rel));
            }
        }
        Ok(Some(unit.finish()))
    }

    fn repo_meta(
        &self,
        source: &SourceConfig,
        root: &Path,
        rules: Option<&OwnershipRules>,
    ) -> Unit {
        let id = &source.id;
        let mut unit = Unit::new(
            keys::unit(id, UnitKind::RepoMeta, "repo"),
            id.clone(),
            UnitKind::RepoMeta,
        );
        let mut node = NodeAssertion::new(
            NodeKind::Service,
            keys::service(id.as_str()),
            source.display_name(),
        )
        .alias(id.as_str())
        .attr(attrs::PATH, root.to_string_lossy().into_owned());
        if let Some(dir) = root.file_name() {
            node = node.alias(dir.to_string_lossy());
        }
        let service = unit.add_node(node);
        // Whoever owns files at the repository root (a catch-all rule) owns the service itself.
        if let Some(rules) = rules {
            const ROOT_PROBE: &str = "README.md";
            unit.nodes.extend(rules.file_parties(ROOT_PROBE));
            unit.edges.extend(
                rules
                    .file_edges(&service, ROOT_PROBE)
                    .into_iter()
                    .filter(|edge| edge.kind == EdgeKind::Owns),
            );
        }
        unit.finish()
    }

    fn scan_repo(
        &self,
        source: &SourceConfig,
        root: &Path,
        report: &mut ScanReport,
        produced: &mut BTreeSet<UnitKey>,
    ) -> Result<bool> {
        if !root.is_dir() {
            report
                .warnings
                .push(format!("{}: not a directory", root.display()));
            return Ok(false);
        }
        let id = &source.id;
        let files = self.walk(source, root)?;
        let (rules, mut batch) = self.load_ownership(id, root, &files, report);
        batch.push(self.repo_meta(source, root, rules.as_deref()));
        for unit in &batch {
            produced.insert(unit.key.clone());
        }
        self.apply_batch(&mut batch, report)?;

        let max = self.config.parser.max_file_bytes;
        for (path, rel) in &files {
            report.files_seen += 1;
            let key = keys::unit(id, UnitKind::File, rel);
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            let stamp = (
                meta.len(),
                meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos() as i128),
            );
            let known = self
                .manifest
                .lock()
                .get(&(id.clone(), rel.clone()))
                .copied();
            if known == Some(stamp)
                && self
                    .store
                    .unit_meta(&key)?
                    .is_some_and(|meta| !meta.deleted)
            {
                report.files_unchanged += 1;
                produced.insert(key);
                continue;
            }
            if meta.len() > max {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            match self.file_unit(id, rel, &text, rules.as_deref()) {
                Ok(Some(unit)) => {
                    produced.insert(unit.key.clone());
                    self.manifest
                        .lock()
                        .insert((id.clone(), rel.clone()), stamp);
                    batch.push(unit);
                    if batch.len() >= BATCH {
                        self.apply_batch(&mut batch, report)?;
                    }
                }
                Ok(None) => {}
                Err(err) => report.warnings.push(format!("{rel}: {err}")),
            }
        }
        self.apply_batch(&mut batch, report)?;
        Ok(true)
    }

    /// The source whose root contains `path`, with the path relative to that root.
    pub fn locate(&self, path: &Path) -> Option<(SourceId, String)> {
        self.config
            .sources
            .iter()
            .filter(|source| source.enabled)
            .filter_map(|source| {
                let rel = path.strip_prefix(&source.location).ok()?;
                Some((source, keys::normalize_path(&rel.to_string_lossy())))
            })
            .max_by_key(|(source, _)| source.location.len())
            .map(|(source, rel)| (source.id.clone(), rel))
    }

    /// The hot path: one file of a source changed (or disappeared) on disk. `detected` is when the
    /// change was noticed; the latency up to the graph update is recorded and published.
    pub fn handle_path_change(&self, path: &Path, detected: Instant) -> Result<bool> {
        let Some((id, rel)) = self.locate(path) else {
            return Ok(false);
        };
        let source = self.source(&id)?;
        if source.kind != SourceKind::Repo {
            self.scan_source(&id)?;
            return Ok(true);
        }
        if rel.starts_with(".git/") || rel.is_empty() {
            return Ok(false);
        }
        if ownership_file_kind(&rel).is_some() {
            self.manifest.lock().retain(|(source, _), _| source != &id);
            self.scan_source(&id)?;
            return Ok(true);
        }
        let key = keys::unit(&id, UnitKind::File, &rel);
        let changed = match std::fs::read_to_string(path) {
            Ok(text) if (text.len() as u64) <= self.config.parser.max_file_bytes => {
                let rules = self.ownership.lock().get(&id).cloned();
                match self.file_unit(&id, &rel, &text, rules.as_deref())? {
                    Some(unit) => self.apply(std::slice::from_ref(&unit))?.changed,
                    None => false,
                }
            }
            Ok(_) => false,
            Err(_) => {
                self.parser.forget(&format!("{id}:{rel}"));
                self.manifest.lock().remove(&(id.clone(), rel.clone()));
                self.remove(std::slice::from_ref(&key))?.changed
            }
        };
        if changed {
            let latency = detected
                .elapsed()
                .min(Duration::from_secs(3600))
                .as_micros() as u64;
            let mut samples = self.latencies.lock();
            if samples.len() >= LATENCY_WINDOW {
                samples.remove(0);
            }
            samples.push(latency);
            drop(samples);
            self.publish(EngineEvent::FileIngested {
                source: id.to_string(),
                path: rel,
                latency_us: latency,
            });
        }
        Ok(changed)
    }

    /// Writes a signed `.contextpack` snapshot of every source (or of `sources`) into the packs
    /// directory and returns its path.
    pub fn export_pack(&self, sources: Option<&[SourceId]>) -> Result<PathBuf> {
        use crate::sync::merkle::MerkleTree;
        use crate::sync::pack::{PackEntry, PackKind, PackWriter};
        let mut writer = PackWriter::new(&self.identity, PackKind::Snapshot)?;
        let mut selected = Vec::new();
        for source in self.store.sources()? {
            if sources.is_some_and(|wanted| !wanted.contains(&source.id)) {
                continue;
            }
            let metas = self.store.unit_metas(&source.id)?;
            writer.namespace(source, None, MerkleTree::from_metas(metas.clone()).root())?;
            selected.push(metas);
        }
        for meta in selected.into_iter().flatten() {
            let unit = if meta.deleted {
                None
            } else {
                self.store.unit(&meta.key)?
            };
            writer.write(&PackEntry { meta, unit })?;
        }
        let dir = self.paths.packs_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("anchor-{}.contextpack", crate::base::now_ms()));
        writer.finish(std::fs::File::create(&path)?)?;
        Ok(path)
    }

    /// Verifies and merges a `.contextpack`: each unit version is applied when it supersedes the
    /// local one. Sources unknown locally are registered with the trust level the pack declares;
    /// locally configured trust levels are never changed. Returns the number of units applied.
    pub fn import_pack(&self, path: &Path) -> Result<usize> {
        use crate::sync::identity::TrustPolicy;
        use crate::sync::pack::{PackLimits, PackReader};
        let mut reader = PackReader::open(
            std::fs::File::open(path)?,
            &TrustPolicy::AnyValid,
            PackLimits::default(),
        )?;
        let known: BTreeSet<SourceId> = self.store.sources()?.into_iter().map(|s| s.id).collect();
        for namespace in &reader.manifest().namespaces {
            if !known.contains(&namespace.source.id) {
                self.store.upsert_source(&namespace.source)?;
            }
        }
        let mut applied = 0;
        while let Some(entry) = reader.next_entry()? {
            let outcome = self.store.apply_remote(&entry.meta, entry.unit.as_ref())?;
            if outcome.changed {
                applied += 1;
            }
            self.absorb(&outcome)?;
        }
        Ok(applied)
    }

    /// Flushes buffered state (the token ledger) to disk.
    pub fn flush(&self) -> Result<()> {
        self.ledger.flush()
    }
}
