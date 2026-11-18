# Working on Anchor

Notes for coding agents (and people) changing this repository. For *using* Anchor from an agent,
see [docs/agents.md](docs/agents.md) and [skills/anchor/SKILL.md](skills/anchor/SKILL.md).

## What it is

A single Rust binary (`anchor`) that ingests repositories, wikis, trackers, code reviews, API
approvals and an org model into one graph, and answers questions against it: SQLite is the system
of record, a compact petgraph index in memory does the traversal, and language models are
optional. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) is the contract between modules; read the
sections for the area you touch before changing it.

## Layout

| Path | What lives there |
|---|---|
| `src/base/` | Vocabulary (node/edge kinds, trust), the unit model, keys, the `Db` handle |
| `src/engine/store/` | `GraphStore`: units, the materialised graph, linker, full-text search, graph versions and the delta log |
| `src/engine/graph/` | In-memory graph, traversal profiles, the trust-weighted pathfinder, the saved snapshot |
| `src/engine/query/` | The question pipeline (intent, seeds, traversal, context, routing) |
| `src/engine/impact.rs`, `why.rs` | `anchor impact` / `hotspots` and `anchor why` |
| `src/engine/ingest/`, `parser/` | Source formats and Tree-sitter parsing |
| `src/mcp/`, `src/web/`, `src/cli/` | MCP server, dashboard and JSON API, command line |
| `src/sync/` | Signed `.contextpack` deltas, Merkle trees, server and client |
| `src/synth/` | The synthetic organisation behind `anchor demo` and the tests' ground truth |
| `tests/` | Integration tests, one file per area; `tests/e2e.rs` scores retrieval against ground truth |

## Before you finish

CI runs exactly these; run them locally and leave them clean:

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

`cargo test` is hermetic (no network, no model) and runs in under a minute once built. Tests
needing the network are `#[ignore]`d; see [docs/development.md](docs/development.md).

## Conventions

- **Lints are a ratchet.** `Cargo.toml` enables `missing_docs`, `missing_debug_implementations` and
  a curated clippy set (no `unwrap` outside tests, no `dbg!`). Fix the code rather than adding
  `allow`s; every public item gets a doc comment and every public type implements `Debug`.
- **Trust is the point.** Any new answer path must keep `source_of_truth` winning over
  `reference_only` (strict authority in `graph::cost`) and must report conflicts rather than hide
  them.
- **No model on the hot path.** Retrieval and direct answers work without any model; a model may
  only add prose on request.
- **Reads that touch both the graph and the tables go through `GraphStore::consistent`.** It
  gives one version of both. Inside it, do not write and do not lock the graph again (parking_lot
  read locks are not re-entrant once a writer waits). Lock order is always database connection,
  then graph.
- **Graph searches carry budgets.** Build `Limits` from `GraphConfig` and add the deadline with
  `Limits::until`; surface `Pathfinder::truncated()` to the caller as `truncated`.
- **Schema changes are migrations.** Add a `Migration` with the next version to the owning module's
  `MIGRATIONS`; never edit an applied one.
- **Blocking work stays off the async runtime.** `Db` and store methods block; call them from
  `spawn_blocking` (or the `*_async` helpers).
- **Tests check behaviour against ground truth.** Prefer a generated world (`synth::generate`) and
  its `ground_truth.json` over hand-written expectations, and add a test that fails without your
  change.
- **Docs move with the code.** A new command or flag updates `docs/commands.md`; a new MCP tool
  updates `docs/agents.md` and the skill; a new design rule updates `docs/ARCHITECTURE.md`.

## Commits and releases

- Commit messages: an imperative summary of what changed and why, in plain sentences. No
  generated-by or co-author trailers.
- Releases are cut by tagging; see [RELEASING.md](RELEASING.md). Do not publish packages or
  deploy anything by hand.
