# Anchor

A local-first context engine and synchronisation daemon. Anchor watches your repositories and
ingests wikis (including comment threads and dismissed discussions), API approval portals, Jira
exports, code-review exports and an organisation model into one entity-relationship graph, then
answers questions such as "Who owns API X?" or "Why is this blocked?" against it.

Every source has an explicit trust level. `source_of_truth` sources (code, CODEOWNERS, approved
API definitions, the org model) always win a conflict against `reference_only` sources (wikis,
tickets, review threads), which are used only as contextual fallback.

Design details are in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Requirements

- Rust 1.88 or newer and a C compiler. No OpenSSL headers, cmake or protoc are needed.
- Linux is the tested platform. Managed Ollama installation is Linux-only; elsewhere install
  Ollama yourself and set `ollama.base_url`.
- No language model is required: without one, Anchor classifies questions with heuristics and
  answers ownership and "why is it blocked" questions directly from the graph.

## Build

```sh
cargo build --release          # binary at target/release/anchor
cargo build --release --no-default-features   # without the Candle embedding backend
```

## Quick start

```sh
anchor init                    # writes a starter config (see "Configuration" for the location)
$EDITOR ~/.config/anchor/anchor.toml          # add [[sources]]
anchor ingest                  # scan every source into the graph
anchor query "Who owns the payments-api service?"
anchor daemon                  # initial scan + file watching + dashboard on http://localhost:7890
```

`anchor setup` runs the interactive wizard instead of editing the file by hand: it detects or
installs Ollama, configures the two model tiers, validates their endpoints and walks through the
sources and their trust levels.

To try Anchor without connecting real systems, generate a synthetic enterprise:

```sh
anchor demo --out /tmp/anchor-demo
anchor --config /tmp/anchor-demo/anchor.toml ingest
anchor --config /tmp/anchor-demo/anchor.toml daemon
```

## Configuration

The config file is `$ANCHOR_CONFIG`, else `$ANCHOR_HOME/anchor.toml`, else
`~/.config/anchor/anchor.toml`. Pass `--config <file>` to any command to override it. Data (the
SQLite database, device keys, managed Ollama, exported packs) lives in `data_dir`, default
`~/.local/share/anchor`.

```toml
mode = "standalone"            # standalone | client | server

[[sources]]
id = "payments-api"
kind = "repo"                  # repo | wiki | api_portal | jira | org_model | code_review
trust = "source_of_truth"      # source_of_truth | reference_only
location = "/src/payments-api"

[[sources]]
id = "eng-wiki"
kind = "wiki"
trust = "reference_only"
location = "/exports/wiki"

[models.system1]               # fast local tier: intent parsing, classification, filtering
provider = "ollama"            # ollama | disabled
model = "llama3.2:3b"
base_url = "http://127.0.0.1:11434"

[models.system2]               # heavy tier: reasoning and synthesis (off until configured)
provider = "anthropic"         # anthropic | openai_compatible | ollama | disabled
model = "claude-opus-5-5"
api_key_env = "ANTHROPIC_API_KEY"   # the NAME of the variable; keys are never stored

[models.routing]
allow_cloud = true
max_cloud_usd_per_day = 5.0

[web]
bind = "127.0.0.1"
port = 7890
```

Every section is optional. Default trust levels are `source_of_truth` for repositories, API
portals and the org model, and `reference_only` for wikis, Jira and code reviews.

### What each source kind reads

| Kind | Location | Contents |
|------|----------|----------|
| `repo` | a checkout | Rust, Python, JavaScript, TypeScript/TSX, Go, Java and C sources; Markdown/RST docs; `CODEOWNERS`, `MAINTAINERS`, `OWNERS`; OpenAPI documents |
| `wiki` | a directory | Markdown (with front matter) and HTML pages; comment threads from `<page>.comments.json` sidecars or embedded markup, including resolved and dismissed threads |
| `api_portal` | a directory | API records with approvals, plus OpenAPI/Swagger definitions |
| `jira` | a directory | issue JSON in Jira REST shape (single issues, arrays or search results) |
| `org_model` | a file or directory | portfolios → domains → components → services, teams and people (YAML, JSON or TOML) |
| `code_review` | a directory | one JSON file per pull request; `scripts/corpus/fetch_reviews.py` exports them from GitHub |

The exact accepted formats are documented at the top of each parser module under
`src/engine/ingest/`.

## Commands

| Command | Purpose |
|---------|---------|
| `anchor setup` | Interactive setup wizard |
| `anchor init [--force]` | Write a starter configuration |
| `anchor ingest [--source ID]` | Scan sources into the graph (incremental) |
| `anchor query "…" [--json] [--local-only] [--retrieval-only]` | Ask a question |
| `anchor daemon [--no-web]` | Scan, watch files, serve the dashboard |
| `anchor status` | Graph and source statistics |
| `anchor spend [--days N]` | Token usage and estimated cost, local vs. cloud |
| `anchor backlog` | Open gap-analysis items |
| `anchor deepen SOURCE [--depth map\|full]` | Ingest a source more deeply |
| `anchor export [-o FILE]` / `anchor import FILE` | Write or merge a signed `.contextpack` |
| `anchor server` / `anchor sync` | Run the central sync server / sync once against it |
| `anchor ollama status\|provision\|teardown\|rebuild` | Manage the local Ollama backend |
| `anchor demo --out DIR` | Generate a synthetic enterprise to try Anchor on |

## Ollama

`anchor ollama provision` uses the server at `ollama.base_url` if one answers. Otherwise it
installs the official build into `data_dir/ollama` (user space, no sudo), starts it, pulls
`ollama.default_model` and verifies a generation. An external server is never stopped or wiped.

`anchor ollama rebuild` is the bootstrap resilience test: it wipes the managed installation and
models, reinstalls, re-pulls and verifies, and prints per-phase timings. `--keep-binary` and
`--keep-downloads` narrow what is wiped. `anchor ollama teardown` only removes.

## Dashboard

`anchor daemon` serves `http://localhost:7890` with: an overview (counts, sources and trust
levels, change-to-graph latency), a graph explorer (search, node detail with provenance, trust-
weighted path finder), a query tester (answer, intent, routing, context, conflicts, gaps), the
backlog, token spend, and snapshot export. It is unauthenticated, binds to loopback and refuses
requests addressed to other host names.

JSON API: `/api/health`, `/api/stats`, `/api/graph/search?q=`, `/api/graph/node/{id}`,
`/api/graph/path?from=&to=`, `/api/graph/hierarchy`, `POST /api/query`, `/api/backlog`,
`POST /api/backlog/{id}/{resolve|dismiss|reopen|retry}`, `/api/spend?days=`,
`POST /api/sources/{id}/rescan`, `POST /api/export`.

## Client/server sync

Run `anchor server` on a central machine (`sync.listen`, default `0.0.0.0:7891`). On each client
set `mode = "client"`, `sync.server_url`, and which sources to `publish` and `subscribe` to, then
run `anchor sync` (or the daemon). Devices exchange signed `.contextpack` deltas; merging is last-
writer-wins per unit, and locally configured trust levels are never overridden by a peer. Set
`sync.auth_token_env` on both sides to require a bearer token.

## Testing

```sh
cargo test --all-features                          # everything hermetic (no network)
cargo test --all-features --test performance -- --nocapture   # budgets, with measurements
cargo clippy --all-targets --all-features -- -D warnings
```

The performance suite asserts: file change → graph mutation p95 under 50 ms, idle CPU ~0 %,
resident memory under 3 GB during 2,000 concurrent queries, and projects the SQLite and
`.contextpack` footprint to 1,000 repositories.

Opt-in tests that need the network:

```sh
ANCHOR_E2E_OLLAMA=1 cargo test --all-features e2e_real_ollama -- --ignored --nocapture
```

downloads the real Ollama build (~1.5 GB) into `target/tmp`, pulls a tiny model, generates, tears
down, rebuilds and verifies again.

`scripts/fetch-corpus.sh` fetches a real-world corpus (Linux, Kubernetes, Grafana, exported pull-
request reviews) into `./corpus` for scale testing.

## Zoom levels (depth)

Each source is ingested at a depth, like map tiles. Everyone can hold the whole organisation at
phone-book depth and go deep only where they work.

| Depth | What is ingested |
|-------|------------------|
| `phone_book` | Repositories: the service and its owners only. Org model and API portal: everything (they are the phone book). Wikis, Jira and reviews: nothing. |
| `map` | Plus files, public types (structs, classes, interfaces, traits, enums, modules) with their doc comments, and document text. No functions or methods, no private items, no call graph, no code previews. |
| `full` | Everything (the default). |

```toml
[[sources]]
id = "payments-api"
kind = "repo"
trust = "source_of_truth"
depth = "phone_book"
location = "/src/payments-api"

[query]
on_shallow = "background"      # block | background | manual
```

When a question touches a source below full depth, the answer always lists it under `shallow`
(in the JSON response, the CLI output and the dashboard), with its depth and what is being done:

- `block`: deepen the source first, then answer from the deeper graph.
- `background` (default): answer now from what is there, and deepen behind it; asking again later
  gives the fuller answer.
- `manual`: answer now and wait for `anchor deepen <source> [--depth map|full]`.

Deepening works from the local copy of the source and needs no server. It is remembered across
restarts and never goes shallower. Pulling deeper tiles from a sync server for sources that are
not on the local disk is not implemented yet.

## Database size

Size follows code volume, not repository count. Measured on the Kubernetes repository (32k files,
364k symbols, 1.15M edges): 1.5 GB database, 4 minutes to ingest, under 1 GB of memory. A typical
40-file service is about 1 MB. Options that trade detail for size:

```toml
[parser]
call_graph = true                    # false: files and symbols only, no call/reference/import edges
keep_unresolved_references = false   # true: keep calls into code outside the graph for later linking

[embeddings]
embed_code = false                   # true: also embed code previews, not only docs and comments
```

## Measured retrieval quality

`cargo test --all-features --test e2e -- --nocapture` scores 166 generated questions with known
answers. Current results: ownership questions ("Who owns API X?") recall 0.99 and top-answer
accuracy 0.94, with stale reference-only claims never winning. Justification questions ("Why is
this blocking?") are weaker: 1.0 recall for blocked APIs and endpoints, 0.5 for blocked tickets,
and near zero for pull-request questions. Multi-hop relationship questions reach 0.37.

## Known limitations

- Questions about a specific pull request, and multi-hop relationship questions ("who leads the
  team that owns the component containing X"), are not yet understood by the heuristic classifier
  and retrieve poorly without a language model.
- Context precision is low (about 0.2 for ownership): the context includes neighbouring evidence
  beyond the answer.
- One provisioning test (`a_system_binary_is_preferred_over_downloading`) failed once in a full
  parallel run and passed on every re-run; it is probably a port race in the test fakes.

- Two graph nodes that already exist are not merged retroactively when a later source reveals
  they are the same entity; `GraphStore::rebuild` re-materialises the graph and heals this.
- Wiki prose is linked to entities it mentions, but ownership stated only in prose is not turned
  into an ownership claim.
- Managed Ollama installation supports Linux only.
