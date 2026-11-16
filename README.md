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
| `code_review` | a directory | one JSON file per pull request; `anchor fetch` exports them from GitHub |
| `issues` | a directory | issue-board and discussion-board exports (GitHub issues and discussions), as written by `anchor fetch` |

The exact accepted formats are documented at the top of each parser module under
`src/engine/ingest/`.

## Commands

| Command | Purpose |
|---------|---------|
| `anchor setup` | Interactive setup wizard |
| `anchor init [--force]` | Write a starter configuration |
| `anchor ingest [--source ID]` | Scan sources into the graph (incremental) |
| `anchor query "…" [--json] [--explain] [--local-only] [--retrieval-only]` | Ask a question (`--explain`: model-written prose) |
| `anchor daemon [--no-web]` | Scan, watch files, serve the dashboard |
| `anchor status` | Graph and source statistics |
| `anchor spend [--days N]` | Token usage and estimated cost, local vs. cloud |
| `anchor backlog` | Open gap-analysis items |
| `anchor enrich [--limit N]` | Summarise and tag pages, tickets, PRs and APIs with the local model |
| `anchor mcp` | Serve the Model Context Protocol on stdio, for agents (Claude Code, Cursor, …) |
| `anchor eval --truth FILE` | Score the engine against questions with known answers |
| `anchor compare DIR [--truth FILE] [--reindex] [--kinds]` | Score Anchor next to plain retrieval and other RAG systems: quality, latency, disk and memory |
| `anchor fetch OWNER/REPO [--issues N] [--discussions N] [--pulls N] [--out DIR]` | Export a GitHub repo's issues, discussions and PR reviews |
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

## Using Anchor from an agent (MCP)

`anchor mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server over stdio,
the interface agent hosts such as Claude Code, Claude Desktop and Cursor speak. It opens the same
database as the CLI and the daemon, and can run alongside the daemon.

Claude Code:

```sh
claude mcp add anchor -- anchor mcp
# or with an explicit configuration file
claude mcp add anchor -- anchor --config /path/to/anchor.toml mcp
```

Any other host, in its MCP server configuration:

```json
{ "mcpServers": { "anchor": { "command": "anchor", "args": ["mcp"] } } }
```

| Tool | Purpose |
|------|---------|
| `ask` | A question in; the answer (when the graph holds one), evidence with trust levels and the path that led to it, resolved conflicts, gaps and shallow sources out. No model call unless `explain` is set |
| `search` | Find entities by name or alias, optionally of one kind |
| `get` | One entity: attributes, asserting sources, relationships, text |
| `path` | The most trustworthy chain of relationships between two entities |
| `deepen` | Ingest a shallow source more deeply; call it when `ask` lists the source under `shallow`, then ask again |
| `status` | Sources with their kind, trust and depth; entity counts |

Entities are addressed by key (`service:billing-api`, as returned by `ask` and `search`) or by
name. Tool failures come back as tool errors (`isError`), so the agent can correct itself.
Protocol revisions 2024-11-05, 2025-03-26 and 2025-06-18 are supported. For hosts that do not
speak MCP, `anchor query --json` and the dashboard's `POST /api/query` return the same data.

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

## What the models do

Models stay off the interactive path. Questions are classified by rules in microseconds and
answered straight from the graph; nothing waits on a model unless you ask for prose.

| Where | What the model does | When |
|-------|---------------------|------|
| Classification | The local System 1 model reads a question only when the rules cannot classify it (multi-hop "who leads the team that owns…" questions). Its reading is cached, so a repeated question is instant. | At question time, as a fallback |
| Answers | `anchor query --explain` (or `query.model_answers = true`) has a model write a prose answer from the retrieved evidence. | Only on request |
| Enrichment | `anchor enrich` has System 1 summarise and tag wiki pages, tickets, pull requests and APIs, and name the entities they are about. Tags become searchable aliases; named entities become links. Stored as a `reference_only` source, so it never overrides an authoritative fact. | In the background, after ingestion (the daemon does it automatically) |

Models are never used to search the graph.

To measure what they change, generate a question set with known answers and score it with and
without a model configured:

```sh
anchor demo --out /tmp/demo --scale small
anchor --config /tmp/demo/anchor.toml ingest
anchor --config /tmp/demo/anchor.toml eval --truth /tmp/demo/ground_truth.json
```

`anchor eval` reports, per question category, how often the intent was read correctly, context
recall, whether the top answer was right, latency per question, and model calls and tokens.

## Issue boards, discussions and pull requests

`anchor fetch owner/repo` exports the most recently updated issues, discussions and pull requests
of a GitHub repository (with their comment and review threads) and prints the `[[sources]]`
entries to add. It uses the GitHub CLI, so run `gh auth login` first; nothing is fetched unless
you run the command or configure a `[[github]]` entry.

```sh
anchor fetch grafana/grafana --issues 100 --discussions 50 --pulls 50
anchor ingest
anchor query "What is the status of grafana#105210?"
```

Re-running `anchor fetch owner/repo` downloads only items that are new or changed upstream. To
have the daemon keep a board current, add it to the configuration; `anchor fetch` with no
repository refreshes every entry.

```toml
[[github]]
repository = "grafana/grafana"
issues = 100
discussions = 50
pulls = 50
interval_minutes = 60          # 0: only on `anchor fetch`
```

Refer to an item as `repo#123` in a question. For `issues` sources, `map` depth ingests titles,
state, labels and descriptions; `full` adds the comment threads.

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

Deepening a source that is on this machine works from the local copy and needs no server. It is
remembered across restarts and never goes shallower.

### Depth with a sync server

A client does not need a local copy of a source to hold it. Depth is a view, not different
content: units travel and are stored whole on every device (so hashes, Merkle roots and merging
are unaffected), and a client materialises into its graph only what its depth for that source
allows. At `phone_book` depth it does not even request the units that depth would not show
(files, tickets, wiki pages, reviews), so a whole organisation costs a few units per repository.

```toml
mode = "client"

[sync]
server_url = "ws://anchor.example.com:7891/sync"
default_depth = "phone_book"   # depth of subscribed sources not configured locally (default: full)
```

A source is *remote* when it is not in the local configuration or its `location` does not exist
on this machine. `anchor sync` brings remote sources in at `default_depth`. When a question
touches one (or you run `anchor deepen <source> --depth map|full`), the client pulls the missing
units from the server, re-materialises the source at the new depth and remembers it. With
`on_shallow = "block"` this happens before the answer, which is then marked `deepened`. Deepening
a remote source with no sync server configured is an error ("source X is not on this machine and
no sync server is configured"). Sources that are local keep using the local copy.

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
answers by node key. Current results: ownership recall 1.00 and top-answer accuracy 0.99, with
stale reference-only claims never winning; multi-hop relationship questions recall 1.00 and
precision 1.00; justification recall 0.66 by key (the neutral, text-based score below is higher,
because the reason is usually returned on the answer rather than as a separate comment node).

## Comparing with other RAG systems

`anchor compare` scores Anchor next to other retrieval systems on the same corpus and questions,
through a format any system can produce:

- `questions.jsonl`: the question plus the names and aliases of each expected answer, and of
  each known-stale answer. No Anchor keys are needed to score.
- `results/<system>.jsonl`: per question, the text contexts retrieved (best first), optionally
  the system's own answer and an answer generated from those contexts by a shared model.
- `results/<system>.meta.json`: index size on disk, peak memory, indexing time.

Every system's contexts are cut to the same budget (6,000 tokens) and matched with the same
string rules. To reproduce:

```sh
anchor demo --out corpus/compare/data --scale medium --seed 7      # 100 repos, 657 questions
mv corpus/compare/data/{anchor.toml,ground_truth.json} corpus/compare/   # keep them out of the corpus
# set data_dir in corpus/compare/anchor.toml, then:
anchor --config corpus/compare/anchor.toml compare corpus/compare/run \
    --truth corpus/compare/ground_truth.json --reindex              # Anchor + plain-retrieval baselines

python -m venv venv && venv/bin/pip install llama-index-core llama-index-embeddings-ollama \
    llama-index-retrievers-bm25 langchain langchain-community langchain-ollama \
    langchain-text-splitters faiss-cpu rank_bm25 lightrag-hku
cd scripts/compare
python run_llamaindex.py ../../corpus/compare/data ../../corpus/compare/run --index-dir /tmp/li
python run_langchain.py  ../../corpus/compare/data ../../corpus/compare/run --index-dir /tmp/lc
python run_lightrag.py   ../../corpus/compare/data ../../corpus/compare/run --workdir /tmp/lr   # hours
python generate.py ../../corpus/compare/run          # same local model answers from each system's contexts
anchor compare ../../corpus/compare/run [--kinds]    # score everything
```

To add another system, write its `results/<name>.jsonl`; `scripts/compare/common.py` has the
helpers.

Results on the 100-repository synthetic enterprise (657 questions; LlamaIndex and LangChain with
their default loaders and splitters and `nomic-embed-text` embeddings; answers generated by
`llama3.1:8b` on a 101-question sample):

| System | Answers in retrieved context | Reader model answers correctly | Tokens per question | ms per query |
|---|---|---|---|---|
| Anchor | 0.99 | 0.89 | 1,232 | 5 |
| LlamaIndex hybrid (vector + BM25) | 0.88 | 0.49 | 3,026 | 35 |
| LlamaIndex BM25 | 0.83 | 0.45 | 3,591 | 0.4 |
| LlamaIndex vector | 0.59 | 0.35 | 2,006 | 34 |
| LangChain hybrid | 0.67 | 0.39 | 1,901 | 19 |
| LangChain BM25 | 0.61 | 0.33 | 2,135 | 1.5 |
| LangChain vector | 0.62 | 0.28 | 1,500 | 18 |
| Plain full-text over Anchor's chunks | 0.45 | 0.19 | 1,855 | 4 |

By category, reader-model answers (Anchor / best other system): ownership 0.90 / 0.37,
justification 0.77 / 0.60, relationships 1.00 / 0.60. On the 12 questions where a stale
reference-only source contradicts the source of truth, the reader led with the correct owner
12 of 12 times on Anchor's context and at most 1 of 12 times on any other system's. Anchor also
answers 0.99 of all questions directly from the graph, without a model.

| System | Index on disk | Peak memory | Indexing time |
|---|---|---|---|
| Anchor | 12.7 MB | 144 MB | 1.8 s |
| LangChain (FAISS + BM25) | 8.7 MB | 200 MB | 43 s |
| LlamaIndex (vector + BM25) | 16.5 MB | 321 MB | 27 s |

The LangChain and LlamaIndex figures leave out the embedding model, which runs in Ollama
(274 MB on disk, plus its memory); Anchor's default embeddings are computed in-process.

The same run scored with the metrics other projects define. None of LlamaIndex, LangChain,
LightRAG, GraphRAG or Ragas publishes speed or footprint figures in its repository, so there are
no upstream numbers to match on those; the quality metrics they define are:

| System | Hit rate | MRR | Precision | nDCG | Context precision |
|---|---|---|---|---|---|
| Anchor | 0.99 | 0.99 | 0.18 | 0.95 | 0.89 |
| LlamaIndex hybrid | 0.88 | 0.40 | 0.12 | 0.50 | 0.35 |
| LlamaIndex BM25 | 0.83 | 0.54 | 0.12 | 0.58 | 0.46 |
| LlamaIndex vector | 0.60 | 0.27 | 0.07 | 0.34 | 0.25 |
| LangChain hybrid | 0.67 | 0.31 | 0.08 | 0.38 | 0.28 |
| LangChain BM25 | 0.61 | 0.28 | 0.07 | 0.35 | 0.26 |
| LangChain vector | 0.64 | 0.30 | 0.08 | 0.37 | 0.27 |

Hit rate, MRR, precision and nDCG are LlamaIndex's retrieval metrics (`RetrieverEvaluator`);
context precision is Ragas' non-LLM variant. Those libraries judge relevance by chunk id; since
every system here chunks differently, a context counts as relevant when it contains an expected
answer. Anchor's MRR is helped by its direct answer being the first thing it returns. Its low
precision (most of what it returns is supporting evidence, not the answer) is a known limitation.
LightRAG evaluates itself differently, with an LLM judge choosing the better of two answers
(`scripts/compare/judge.py` reproduces its prompt).

How to read these numbers:

- The corpus and questions are generated by this project and exercise what Anchor was built
  for: ownership, trust conflicts and typed multi-hop relationships. Free-form "how does this
  work" questions over prose, where vector search is strong, are not in the set.
- Anchor's retrieval was improved after the first comparison (0.81 before, 0.99 after). A second
  corpus generated with a different seed and never inspected scores the same, but it uses the
  same question templates, so the result may not carry over to differently worded questions.
- "Answers in retrieved context" only checks that the answer's name appears in the text; the
  reader-model column is the better measure of whether the context is usable.
- The other systems ran with out-of-the-box settings; tuned chunking, a reranker or a stronger
  embedding model would raise their scores.

## Known limitations

- Relation chains are read by rules: a multi-hop question worded outside the phrases in
  `src/engine/router/relation.rs` falls back to neighbourhood retrieval (or to the System 1
  model when it is enabled).
- A file or endpoint name shared by several repositories is answered as ambiguous unless the
  question names the service.
- Context precision is low (about 0.2 for ownership): the context includes neighbouring evidence
  beyond the answer.
- One provisioning test (`a_system_binary_is_preferred_over_downloading`) failed once in a full
  parallel run and passed on every re-run; it is probably a port race in the test fakes.

- Two graph nodes that already exist are not merged retroactively when a later source reveals
  they are the same entity; `GraphStore::rebuild` re-materialises the graph and heals this.
- Wiki prose is linked to entities it mentions, but ownership stated only in prose is not turned
  into an ownership claim.
- Managed Ollama installation supports Linux only.
