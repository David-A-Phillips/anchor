# Changelog

## 0.1.2-beta

### Breaking changes

- **Ownership answers name only the nearest owner.** The team that owns the enclosing component,
  the domain lead and other owners further away are now `evidence`, not `answer`, unless they
  are as close and as trusted as the nearest one (two CODEOWNERS of one file are both answers).
  Code that collected every `answer` item gets fewer, more precise results.
- **`route.reason` in query JSON is readable text** (for example "confident lookup answered
  from retrieved facts"), not an internal enum name.
- **A question about something the graph does not hold gets no direct answer.** Before, a fuzzy
  match could produce a confident answer about a different entity; now the answer says nothing
  is called that, confidence is at most 0.3, and the matches are returned as evidence.
- **`anchor why path:LINE` past the end of the file is an input error**, instead of falling back
  to the file's history.
- **The sync server listens on `127.0.0.1:7891` by default**, so a fresh install is not reachable
  from the network. Set `[sync] listen = "0.0.0.0:7891"` on a team server (the Docker image and
  the examples in `docs/hosting.md` already do).

### New

- **`anchor impact`**: everything that depends on any of several files, symbols, services, APIs,
  teams or people, nearest first with the chain that links it, and the owners to notify.
  `--diff [REV]` takes the files changed in git; `--markdown` writes a pull-request comment.
  A team or person stands for everything it owns; a component, domain or portfolio for the
  services it contains. Also an MCP tool and `/api/impact`.
- **`anchor hotspots`**: the most depended-on services (or components, APIs, files, symbols),
  their owners and, from git history, who changed them, flagging `no_owner`,
  `single_person_owner`, `reference_only_owner` and `single_committer`. Also an MCP tool and
  `/api/hotspots`.
- **Change impact on pull requests**: `examples/github/anchor-impact.yml` and
  [docs/ci.md](docs/ci.md).
- **Query budgets**: graph searches stop at `[query] timeout_ms` (default 10 s) or
  `[graph] max_visits` (default 200,000) and say `truncated: true`; MCP calls that run past three
  times the budget return an error instead of hanging.
- **`anchor demo` builds git histories** for the sample repositories, so `anchor why` works on
  the demo (`--no-history` for the exact benchmark corpus).
- `anchor source list|add|remove`, an MCP `ingest` tool, and a skill installed by
  `anchor mcp install`.
- `AGENTS.md` for coding agents working on this repository.
- **Wiki pages are linked by the titles they name**, without a model, after every full scan
  (`[graph] title_links`, on by default), and kept current by the watcher as pages change. Links
  through titles that many pages name are weighted down. On 2WikiMultiHopQA, with the ranking
  below, recall@5 goes from 66.7 to 91.0.
- **Open questions are ranked by fused search and their links**: node-name, full-text and (with
  a neural embedding model) vector results are merged by reciprocal rank fusion, and the best
  candidates pass relevance along their links; a node name counts only when the question contains
  it whole. Questions seeking an owner, dependency or other answer kind are ranked as before.
  Average recall@5 on MuSiQue, 2WikiMultiHopQA and HotpotQA: 59.4 to 72.1 (BM25 61.1).
- **`anchor enrich --links-only`**: the model only names what each text refers to, about a quarter
  of the output of a full pass. Names the model gives now also link to wiki pages, and up to
  eight calls run at once.
- **Embedding models are used as trained**: BGE English models pool the `[CLS]` state and prefix
  questions with their instruction; E5 models prefix `query: ` and `passage: `.

### Fixed

- **Long-running processes see other processes' writes.** An `anchor mcp` server or dashboard
  used to keep the graph it started with until restarted; now every write is versioned and
  logged, and readers replay what they missed before answering.
- **Reads are consistent**: a question, impact or hotspot report reads the in-memory graph and
  the database at the same version, even while another process writes.
- **Faster start-up**: the graph is saved to `graph.snapshot` and only the changes since are
  replayed (a missing or damaged file means a rebuild); loading from the database no longer runs
  one query per node.
- **A local model server can no longer exhaust memory.** Ollama calls ask for a fixed context
  window (`context_tokens` per model tier, default 8,192) instead of the server's default, and an
  Anchor-managed server starts with `[ollama] num_parallel` (4) and `context_tokens` (8,192).
  Before, eight parallel requests on a server defaulting to 32,768 tokens reserved more than a
  24 GB GPU holds.
- **Re-ingesting reports only what changed**: unchanged files no longer count as applied units,
  and the title-link pass is skipped when no page changed.
- **Interrupted enrichment keeps its work**: results are saved every 128 nodes, and the next run
  skips them.
- **The install script works**: it refused its own default repository, and could not find a
  version while every release is a pre-release.
- `anchor init` wrote `sources = []`, so following its own advice to add a `[[sources]]` table
  broke the configuration.
- Each question waited about 5 s when no local model server was running (closed loopback ports
  time out under WSL); the connection attempt is now capped at 250 ms, and new configurations
  turn the local model off when none is running.
- Commands run without a configuration explain how to start; a folder of Markdown pages is added
  as a wiki; error messages no longer repeat their category; invalid values list the valid ones.

### Upgrading

Databases from 0.1.1 are migrated on first open (one new table pair for graph versions); the
first start builds the graph from the tables and saves `graph.snapshot`.

## 0.1.1-beta

Windows service host, pre-release packaging, release workflow fixes.
