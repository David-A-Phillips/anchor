# Anchor architecture

Anchor is a local-first context engine and synchronisation daemon. It watches repositories and
ingests wikis (with comment threads and dismissed discussions), API approval portals, trackers and
an organisation model into one entity-relationship graph, and answers questions against that graph
by combining a fast local model (System 1) with a heavy reasoning model (System 2).

This document is the contract between modules. Code in `src/base/` is the machine-checked part of
the same contract.

## 1. Operating modes

| Mode | What runs |
|------|-----------|
| `standalone` | File watcher, ingestion, local SQLite + in-memory graph, query engine, dashboard. |
| `client` | Everything in standalone, plus a sync client exchanging signed `.contextpack` deltas with a central server over WebSocket or gRPC. |
| `server` | The 24/7 central sync server: verifies and merges deltas from clients, serves them to subscribers. It is itself an Anchor engine, so it can also answer queries and serve the dashboard. |

## 2. Data flow

```
 sources                units                      system of record            traversal
┌────────────┐  parse  ┌──────────────┐  apply   ┌───────────────────┐ delta ┌──────────────┐
│ repos      │────────▶│ Unit         │─────────▶│ GraphStore        │──────▶│ ContextGraph │
│ wiki       │         │  nodes       │          │ (SQLite)          │       │ (petgraph)   │
│ API portal │         │  edges       │          │  units, nodes,    │       │ compact,     │
│ Jira       │         │  chunks      │          │  edges, chunks,   │       │ trust-aware  │
│ org model  │         └──────┬───────┘          │  aliases, FTS     │       │ pathfinding  │
└────────────┘                │                  └─────────┬─────────┘       └──────┬───────┘
                              │ same bytes                 │                        │
                              ▼                            ▼                        ▼
                      ┌──────────────┐             ┌──────────────┐         ┌──────────────┐
                      │ .contextpack │◀── sync ───▶│ vector index │────────▶│ QueryEngine  │
                      │ Merkle delta │             │ (embeddings) │         │ S1 → S2      │
                      └──────────────┘             └──────────────┘         └──────┬───────┘
                                                                     ledger ◀──────┴─────▶ backlog
```

SQLite is the system of record. The in-memory graph is a compact index (ids, kinds, trust, weights)
kept in lockstep with it: the store applies every `GraphDelta` to the graph as part of the write
that produced it (§4.7). Names, attributes and text live only in SQLite and are fetched for the
handful of nodes that end up in an answer.

## 3. Module map

| Path | Responsibility |
|------|----------------|
| `src/base/` | Vocabulary (`NodeKind`, `EdgeKind`, `TrustLevel`, ...), the unit data model, keys and alias normalisation, hashing, HLC, the `Db` handle, and the `LanguageModel` / `UsageSink` / `Embedder` contracts. |
| `src/config/` | `anchor.toml` model, loading/saving/validation, the setup wizard, Ollama provisioning (`config::provision`). |
| `src/ollama/` | Ollama HTTP API client (shared by provisioning, the System 1 provider and the Ollama embedder). |
| `src/engine/parser/` | Incremental Tree-sitter parsing and symbol extraction; builds file units. |
| `src/engine/ingest/` | Format parsers producing units: wiki, code review, docs, Jira, API portal, ownership files, org model. |
| `src/engine/store/` | `GraphStore`: units, materialised graph, linker, chunks, full-text search. |
| `src/engine/graph/` | `ContextGraph` (petgraph) and trust-weighted pathfinding / conflict resolution. |
| `src/engine/embed/` | Embedders (hash, Candle, Ollama) and the vector index. |
| `src/engine/router/` | Model providers, intent classification, System 1 / System 2 routing. |
| `src/engine/pipeline/`, `watcher/` | Source scanning, unit application, notify-based watching. |
| `src/engine/query/` | The query engine. |
| `src/sync/` | Device identity, Merkle trees, `.contextpack`, merge rule, WebSocket and gRPC transports, server and client. |
| `src/backlog/` | Gap-analysis backlog table and background worker. |
| `src/metrics/` | Token ledger, pricing, latency statistics, process resource sampling. |
| `src/web/` | Embedded dashboard on `http://localhost:7890`. |
| `src/cli/` | The `anchor` command-line interface. |

Each module owns its SQLite tables (declared as `MIGRATIONS` next to the code and registered in
`src/schema.rs`) and its configuration section (a `config.rs` inside the module, composed into
`config::AnchorConfig`).

## 4. Data model

### 4.1 Sources and trust

A *source* is one configured `[[sources]]` entry: a repository, a wiki snapshot, an API portal
export, a Jira export, an org model, a code-review export. Every source carries an explicit
`TrustLevel`:

* `SourceOfTruth` — the authoritative baseline (code, approved API definitions, the org directory);
* `ReferenceOnly` — contextual fallback (wikis, discussions, tickets).

Trust belongs to the source, never to an individual fact. Everything a source asserts inherits the
source's trust level; changing the level of a source re-weights all of it without re-ingesting.

### 4.2 Units

Everything is ingested as `Unit`s (`src/base/model.rs`). A unit is the atom of ingestion,
replacement, content addressing and synchronisation: one source file, one wiki page with its
threads, one ticket, one API definition with its approvals, one ownership file, one org model
document. A unit *asserts*:

* **nodes** (`NodeAssertion`): key, kind, display name, aliases, attributes;
* **edges** (`EdgeAssertion`): `from --kind--> to` with a confidence weight and attributes, whose
  endpoints are either a known key (`NodeRef::Key`) or a name to resolve (`NodeRef::Lookup`);
* **chunks** (`Chunk`): text attached to a node the unit asserts.

Units are pure content: no timestamps, no row ids, no trust. A unit in canonical form
(`Unit::canonicalize`) has a stable BLAKE3 `content_hash` over its canonical JSON encoding. Applying
a unit **replaces** whatever the previous version of the unit with the same key asserted.

Versioning is separate: a `UnitVersion` is `(Hlc, DeviceId)`, and `UnitMeta` (key, source, kind,
content hash, version, tombstone flag) is what devices compare. `UnitMeta::supersedes` is the merge
rule: last writer wins by hybrid logical time, ties broken by origin then hash, so all devices
converge on the same version regardless of arrival order. Deleting a unit leaves a tombstone.

### 4.3 Node identity

Keys are built **only** through `base::keys` (see the table in that module). Two rules decide
whether two assertions describe the same node:

1. Assertions with the same key are the same node.
2. For kinds where `NodeKind::merges_by_alias()` is true (Portfolio, Domain, Component, Service,
   Team, Person, ApiSpec), assertions that share an *identity alias* are the same node even when
   their keys differ. The identity aliases of an assertion are the body of its key (the part after
   the prefix) and each entry of `aliases`, all normalised with `keys::normalize_alias`. Identity is
   the transitive closure of alias sharing: a node is a connected component of assertions. Removing
   the assertion that bridged two groups splits the node again.

All other kinds are identified by key alone; their aliases only serve lookups.

A materialised node has: a stable row id (`NodeId`), a primary key (the lexicographically smallest
key among its assertions), every asserted key as a secondary key, and merged content. Merging is
by authority: assertions are ordered by (source trust descending, unit version descending, unit key
ascending); the first assertion provides the display name, and for each attribute the first
assertion that sets it wins. The node's trust is the greatest trust among its assertions. Aliases
are the union. A node exists exactly as long as at least one live unit asserts it.

Producers therefore assert every entity they talk about, however little they know (a CODEOWNERS
file asserts a Team with just a name; `engine::ingest::party` builds such assertions), and the
authoritative directory fills in the rest.

### 4.4 Edges and the linker

An edge assertion materialises when both endpoints resolve:

* `NodeRef::Key(k)` resolves to the node having `k` as primary or secondary key.
* `NodeRef::Lookup { kinds, text, scope }` resolves by alias. For each kind in `kinds`, in order,
  the candidates are the nodes of that kind having `normalize_alias(kind, text)` among their
  aliases (name included). The first kind with candidates decides: a single candidate wins; among
  several, the candidate closest to `scope` wins (same source and same path, then same source and
  nearest shared directory, then same source); if the best rank is shared by several candidates the
  lookup is ambiguous and does not resolve.

An unresolved edge assertion is kept **pending** and re-evaluated whenever the set of nodes or
aliases it could match changes, so the materialised graph never depends on ingestion order.
Self-loops are dropped.

Materialised edges are unique per `(from, to, kind)`. Each unit asserting the edge is a *claim*
(kept for provenance and attributes); the edge's trust is the greatest trust among its claims and
its weight the greatest weight. The edge disappears with its last claim or either endpoint.

**Derived links.** Some links need the whole corpus. After a full scan, `engine::title_links`
matches every wiki page title (case-sensitive, on word boundaries, at least four characters,
naming at most three pages) against the text of every other page and records the matches as
links of weight 0.75, scaled by `min(1, 10 / pages naming the title)` so common titles carry
little relevance, as `Mentions` edges in one `Links` unit per linking page, under the built-in
`reference_only` source `anchor-title-links`. The watcher keeps them current: after a wiki page
changes, only that page and the pages whose text contains its title (found by a full-text phrase
search) are recomputed. Enrichment (`anchor enrich`) records the entities a model names the same
way under `anchor-enrichment`. Both are ordinary units: rebuilt as pages change, removed with them,
and never able to override an authoritative fact.

Conflicting facts are therefore distinct edges with distinct provenance — CODEOWNERS (SourceOfTruth)
says team A owns the API, a stale wiki page (ReferenceOnly) says team B does — and are resolved at
query time by trust-weighted traversal (§5), never by discarding data at ingestion.

**Invariant.** The materialised graph (nodes with their keys, names, attributes, trust; edges with
kind, trust, weight; chunks) is a pure function of the set of live unit versions and source trust
levels. Any sequence of applies, updates and removals that ends with the same live units yields the
same graph, up to row ids. This is property-tested.

### 4.5 Chunks

Chunks are the text that gets full-text indexed, embedded and quoted: symbol signatures and doc
comments, wiki sections, comment bodies, ticket descriptions, approval rationales. A chunk belongs
to a node asserted by the same unit. Code chunks are bounded previews, not whole files: the working
tree stays the source of full code.

### 4.6 Well-known attributes and conventions

Attribute names that drive behaviour are constants in `base::attrs`. In particular:

* Thread nodes carry `thread_status` = `open` | `resolved` | `dismissed`, plus `closed_by` and
  `close_reason` when known. Dismissed threads are preserved, not dropped: they are often the
  answer to "why".
* Approval nodes carry `decision` = `approved` | `rejected` | `changes_requested` | `pending`,
  `blocking` (bool) and `reason`.
* `Blocks` edges carry `reason` when the source states one.
* Lookup-friendly aliases: API endpoints register `"<METHOD> <route>"`; files register their
  repository-relative path; symbols register their simple and qualified names; tickets register
  their key; wiki pages register their title.
* Free text is linked to entities with `engine::ingest::mentions` (candidate `Mentions` edges with
  lookups), so wiki comments, tickets and API descriptions attach to code components and API
  definitions once those exist.

### 4.7 `GraphDelta`, graph versions and consistent reads

Every store mutation returns a `GraphDelta` (nodes upserted/removed, edges upserted/removed, by row
id). Application order: nodes upserted, edges removed, edges upserted, nodes removed.

**Versions and the delta log.** A write that changes the graph increments the *graph version*
(`graph_state`) and appends its encoded delta to `graph_log`, in the same transaction. The
database also carries a random *epoch* that identifies its graph history.

**The mirror.** The engine's `GraphStore` holds the shared in-memory graph as its *mirror*. A write
takes the mirror's write lock before it commits and applies the delta before releasing it, so
deltas reach the mirror in commit order and no reader ever sees the tables ahead of the graph. The
graph is stamped with the epoch and version it reflects.

**Consistent reads.** `GraphStore::consistent(f)` read-locks the mirror, then opens a SQLite read
transaction on a connection pinned to the thread (`Db::snapshot_read`); every store read inside
`f` sees that one snapshot, which is exactly the state the graph reflects. Query retrieval,
`impact` and `hotspots` run this way. Writing inside `f` is an error, not a deadlock.

**Other processes.** A daemon, an MCP server and a CLI command can share one database. When a
consistent read finds the store's version ahead of its graph (another process wrote), it catches
up first: it replays the logged deltas since its version, or rebuilds the graph from the tables
when the epoch differs or the log no longer reaches back that far (`MAX_REPLAY`, or pruned).

**The saved graph.** `graph.snapshot` in the data directory is the in-memory graph written to
disk with its epoch and version (fixed-width binary, checksummed, written atomically). On open
the engine loads it and catches up from the log as above; a missing, corrupt or foreign snapshot
just means a rebuild. It is saved on `flush` (after ingest commands), by the daemon every 500
versions, and after a start-up that had to replay or rebuild. Saving trims the log to the last
1,000 entries behind the snapshot, so other processes can still replay. Deleting it is always
safe.

## 5. Traversal and trust weighting

Pathfinding runs on the in-memory petgraph. Requirements:

* Edges are traversable in both directions; cost depends on edge kind and direction. A *traversal
  profile* lists the `(edge kind, direction)` pairs that may be followed and their base costs; pairs
  not listed are not followed. Profiles keep paths meaningful (an ownership question follows
  containment, ownership and implementation edges, not call chains).
* Step cost = base cost × trust multiplier of the edge × trust multiplier of the node entered
  ÷ edge weight. `SourceOfTruth` has a low multiplier, `ReferenceOnly` a high one
  (`graph.trust` in the configuration).
* With `strict_authority` (default) path costs compare lexicographically: first the number of
  `ReferenceOnly` elements on the path, then the weighted cost. A path built only from authoritative
  data therefore beats any path that relies on reference material, however short: authoritative data
  always wins a conflict, and reference material is used only when no authoritative path exists.
* Searches are bounded by `max_hops` and `max_cost`, support multiple seeds, and return paths
  (node and edge sequences) so answers can cite their evidence.
* Every search also has a budget: at most `graph.max_visits` settled nodes, and a deadline
  (`query.timeout_ms` from the start of the question). A search that hits either stops, keeps
  what it found, and marks the pathfinder `truncated`; responses carry `truncated: true` and a
  `truncated` gap, so callers (agents especially) are never left waiting on a pathological
  question. The MCP server additionally fails a read-only tool call that runs past three times
  `query.timeout_ms` with an explanation.
* Conflict resolution groups candidate answers to a single-valued question, picks the winner by the
  rule above and reports the overridden candidates with their provenance.

## 6. Query pipeline

Questions about one entity go through the pipeline below. Two questions about the shape of the
graph have their own entry points (`engine::impact`), on the same consistent reads and budgets:

* **Impact** (`anchor impact`, MCP `impact`, `/api/impact`): many targets at once (files, symbols,
  services, APIs; `--diff REV` takes the files changed in git) seed one search with the *impact*
  profile: dependency edges backwards, implementation forwards to an API and on to its consumers,
  and containment upwards to the enclosing file, service or API (never downwards, so siblings of a
  changed file are not implicated). A file target also seeds the symbols it contains. Each
  dependent keeps the cheapest path to its nearest target; owners of the targets and the listed
  dependents are found with the ownership profile and grouped.
* **Hotspots** (`anchor hotspots`, MCP `hotspots`, `/api/hotspots`): every dependency edge is lifted
  to a granularity (service by default, or component, API, file, symbol) by climbing containment;
  entities are ranked by distinct dependents, each with its nearest owner and risk flags
  (`no_owner`, `single_person_owner`, `reference_only_owner`); owners are ranked by how many
  entities they own directly.

1. **System 1 (local, fast).** Intent parsing and query classification (`QueryIntent`: intent kind,
   entity mentions, target kinds, complexity), by the local model with a deterministic heuristic
   fallback. Initial graph filtering: entity mentions are resolved through the alias index,
   full-text search and the vector index into seed nodes, optionally re-ranked by the local model.
   An *open* question (nothing resolved, no answer kind sought) starts instead from its searches
   (`query::fusion`): node names the question contains whole, full text and, with a neural
   embedding model, vectors, merged by reciprocal rank fusion; each of the five best candidates then passes part of its score
   along its links, so a passage linked from a strong match can outrank a weak match (a
   one-step personalised PageRank). That ranking leads the evidence. Questions seeking an answer
   kind keep their best name matches as seeds in order, since the answer is the nearest such
   node.
2. **Traversal.** Trust-weighted search from the seeds with the profile for the intent; conflicts
   resolved; evidence paths and chunks assembled into a token-budgeted context bundle with a
   confidence score.
3. **Routing.** Simple, confident lookups are answered locally. Complex or low-confidence queries
   escalate to **System 2 (cloud, heavy)** for reasoning, context expansion (bounded rounds of
   "what else do you need?") and synthesis. Cloud use is subject to `models.routing`.
4. **Ledger.** Every model call is reported to the `UsageSink`; the SQLite token ledger records
   prompt/completion tokens, estimated cost, latency, tier (System 1/2) and locality (local/cloud),
   and what a local call would have cost in the cloud.
5. **Gap analysis.** Low-confidence answers, unresolved entities and missing neighbours (an API
   with no owner) become backlog items; a background worker expands context for them.

## 7. Synchronisation

* The synchronised object is the unit version. A namespace is a source id.
* Each namespace has a Merkle tree over `unit key → UnitMeta::version_hash`. Two devices compare
  roots, descend only into differing subtrees and exchange only the units that differ.
* A `.contextpack` is a signed container: a manifest (format version, origin device and public key,
  per-namespace source descriptor, base root → new root, counts, payload hash) and a
  zstd-compressed payload of unit versions (metadata plus canonical unit, or a tombstone). A pack
  with a base root is a delta; without one it is a full snapshot. The manifest is signed with the
  device's Ed25519 key; the payload is covered by its BLAKE3 hash in the manifest; every unit is
  checked against its content hash on import.
* Merging is per unit with `UnitMeta::supersedes`: commutative, associative, idempotent.
* Imported units go through the same `GraphStore` apply path as local ingestion.
* Transports: WebSocket and gRPC carry the same protocol (hello, tree comparison, pull, push,
  change notification). The server authenticates clients by bearer token and verifies signatures
  against trusted device keys.

## 8. Storage rules

* One `Db` (`base::db`) shared by all modules: a single write connection plus pooled read-only
  connections, WAL mode. All `Db` methods block; from async code use the `*_async` variants or a
  dedicated thread. Never call a `Db` method inside a closure passed to another `Db` method.
* Lock order is always the database connection first, then the in-memory graph. Code that holds
  the graph lock must not wait on the write connection, and code inside
  `GraphStore::consistent` must not lock the graph again.
* Use `prepare_cached` on hot paths and batch writes in one transaction.
* Row ids are local. Anything exported, hashed or synchronised uses natural keys.
* Timestamps are `i64` milliseconds since the Unix epoch (`base::time::now_ms`).

## 9. Performance budgets

| Budget | Limit |
|--------|-------|
| File change detected → graph mutated (store committed and in-memory graph updated) | < 50 ms |
| CPU while idle (daemon running, nothing changing) | ~0 %: no polling loops, only event-driven wake-ups and coarse timers |
| Resident memory under heavy query load | < 3 GB |
| In-memory graph | tens of bytes per node/edge; no strings |

Budgets are asserted by tests. The dev/test profile optimises dependencies fully for this reason.

## 10. Engineering conventions

* Rust 2024, `unsafe_code = forbid`. `cargo fmt`, `cargo clippy --all-targets --all-features --
  -D warnings` and `cargo test --all-features` must be clean: no warnings.
* No `unwrap()` outside tests; `expect()` only for invariants that cannot fail, with a message
  saying why. Malformed input, missing files and unreachable services are errors, never panics.
* Errors: `anchor::Error` / `anchor::Result`; modules may define local error enums and convert with
  `From` or `Error::module`.
* Async runtime: Tokio. CPU-bound or blocking work (SQLite, parsing, embedding, compression) never
  runs on the async executor.
* Logging: `tracing`. No `println!` outside the CLI and the wizard.
* HTTP clients are built with `base::net` (rustls + ring). Ollama is reached only through
  `ollama::OllamaApi`.
* Secrets (API keys, tokens) are read from environment variables named in the configuration; they
  are never written to disk or logs.
* Tests are hermetic: no network, no dependence on the host's installed software, temporary
  directories via `tempfile`, HTTP services faked with in-process Axum servers on port 0. Never
  rely on "connection refused" being fast: on some hosts (WSL2 among them) a connection to a closed
  loopback port is silently dropped, so anything probing an absent service needs an explicit short
  timeout. Tests must not mutate process-global state (environment variables, current directory).
* Public items carry rustdoc. Comments explain constraints and reasons, not mechanics.
