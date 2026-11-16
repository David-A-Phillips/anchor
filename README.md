# Anchor

Anchor builds one graph out of your repositories, wikis, trackers, code reviews, API approvals
and organisation chart, and answers questions against it: who owns this, why was that blocked,
what depends on what, and why is this line of code the way it is.

- **Local first.** One binary and a SQLite file. No server, no model and no network are needed
  to ingest and answer.
- **Trust aware.** Every source is `source_of_truth` (code, CODEOWNERS, approved API definitions,
  the org model) or `reference_only` (wikis, tickets, review threads). When they disagree the
  authoritative source wins and the answer says so.
- **Fast.** Questions are classified by rules and answered from the graph in about 6 ms. Models
  are optional and never on the path of an ordinary question.
- **Made for agents.** A Model Context Protocol server gives Claude Code, Cursor and other hosts
  the same answers, with evidence and trust levels.
- **Shareable.** A team server syncs the graph between machines; each machine holds each source
  only as deep as it needs.

On a 735-question benchmark over a 100-repository synthetic organisation, a reader model given
Anchor's context answered 85% correctly, against 49% with the best LlamaIndex configuration, at
a fraction of the tokens and indexing time. The corpus is synthetic and favours Anchor's
question types; see [docs/benchmarks.md](docs/benchmarks.md) for method, data and caveats.

## Contents

- [Install](#install)
- [Run it locally](#run-it-locally)
- [Connect your data sources](#connect-your-data-sources)
- [Models and third-party APIs](#models-and-third-party-apis)
- [Use it from an agent](#using-anchor-from-an-agent-mcp)
- [Host it for a team](#host-it-for-a-team)
- [Commands](#commands)
- [More documentation](#more-documentation)

## Install

```sh
sudo apt-get install anchor-context      # Debian, Ubuntu (after adding the apt repository)
sudo dnf install anchor-context          # Fedora, RHEL family (after adding the repository)
brew install David-A-Phillips/tap/anchor            # macOS, Linux
curl -fsSL https://raw.githubusercontent.com/David-A-Phillips/anchor/main/scripts/install.sh | sh
cargo build --release                    # from source: Rust 1.88+ and a C compiler
```

The package is called `anchor-context` (other distributions' `anchor` packages are unrelated);
the command is `anchor`. [docs/install.md](docs/install.md) has the repository setup for apt and
dnf, upgrade and removal, running as a systemd or Homebrew service, Docker, and Windows.

No release has been published yet, and the repository is private, so the package commands above
do not work until a release exists and its assets are publicly downloadable;
[RELEASING.md](RELEASING.md) lists what a release needs. The `.deb`, `.rpm` and Homebrew formula
have been built and installed in local containers on Linux x86_64; macOS, Windows and arm64 have
not been built.

## Run it locally

Try it on a generated organisation first; nothing else needs to be set up:

```sh
anchor demo --out /tmp/anchor-demo
anchor --config /tmp/anchor-demo/anchor.toml ingest
anchor --config /tmp/anchor-demo/anchor.toml query "..."   # `anchor demo` prints a question to try
anchor --config /tmp/anchor-demo/anchor.toml daemon      # dashboard on http://localhost:7890
```

On your own code:

```sh
anchor init                              # writes ~/.config/anchor/anchor.toml
$EDITOR ~/.config/anchor/anchor.toml     # add [[sources]]; see the next section
anchor ingest                            # scan every source into the graph
anchor query "Who owns the payments-api service?"
anchor why src/ledger/charge.rs:120      # the commits, review and ticket behind a line
anchor daemon                            # keep the graph current as files change; serve the dashboard
```

`anchor setup` is an interactive wizard that does the same and can install a local model.

The config file is `$ANCHOR_CONFIG`, else `$ANCHOR_HOME/anchor.toml`, else
`~/.config/anchor/anchor.toml`; `--config <file>` overrides it for one command. The database,
device keys and exported packs live in `data_dir` (default `~/.local/share/anchor`).

### Dashboard

`anchor daemon` serves `http://localhost:7890` with: an overview (counts, sources and trust
levels, change-to-graph latency), a graph explorer (search, node detail with provenance, trust-
weighted path finder), a query tester (answer, intent, routing, context, conflicts, gaps), the
backlog, token spend, and snapshot export. It is unauthenticated, binds to loopback and refuses
requests addressed to other host names.

JSON API: `/api/health`, `/api/stats`, `/api/graph/search?q=`, `/api/graph/node/{id}`,
`/api/graph/path?from=&to=`, `/api/graph/hierarchy`, `POST /api/query`, `/api/backlog`,
`POST /api/backlog/{id}/{resolve|dismiss|reopen|retry}`, `/api/spend?days=`,
`POST /api/sources/{id}/rescan`, `POST /api/export`.

### Why is this code the way it is (`anchor why`)

`anchor why` is a deeper git blame. It blames the lines you ask about, then follows each
responsible commit through the graph and reports the whole trail:

```
$ anchor why src/charge.rs:3
ledger:src/charge.rs lines 3-3 (by blame)

commit 4f2a9c1e07 by Alice Smith on 2025-03-01 — 1 line(s)
  PAY-12: retry charges with a bounded backoff (#7)
  | The gateway drops one request in a thousand under load.
  pull request pr:ledger#7 "Retry charges" [merged]
    Implements PAY-12.
    - Review of ledger#7 by bob: dismissed: Unbounded retries will hammer the gateway. — dismissed: Dismissed: the retry count is capped at three in the follow-up commit. (by carol)
  ticket PAY-12 "Charges fail intermittently under load" [done]
    Roughly 0.1% of charge requests are dropped by the gateway.
  - wiki_page "Gateway incident review" [reference_only] discusses PAY-12
```

The target is `path`, `path:line`, `path:start-end`, `source:path` (when the same path exists in
several repositories) or a symbol name. Blame runs on demand with the `git` command line, so it
works at any depth and nothing is stored per line. Where the trail ends early (a commit pushed
without a pull request, a ticket that was never ingested) the report says so. The same report
is available to agents as the `why` MCP tool. No model is involved.

Repositories held at full depth also get their recent history in the graph: each commit is a
node linked to its author, the files it changed, and the tickets (`PAY-123`) and pull requests
(`#123`) its message names, and is tagged with the kind of change it announces (`fix`, `revert`,
`feature`, `security`, ...). Configure with:

```toml
[history]
enabled = true
max_commits = 2000            # most recent commits per repository; older ones are still reached by blame
max_files_per_commit = 30     # larger commits keep the count, not the file links
```

For the trail to reach reviews and tickets, ingest them too: `anchor fetch David-A-Phillips/REPO` for GitHub
pull requests, a `jira` source or `anchor fetch-atlassian` for tickets and wiki pages.

## Connect your data sources

Each source is one `[[sources]]` entry with a kind, a location and a trust level. Local
directories are read in place; third-party systems are exported into a directory first by a
fetch command, which the daemon can repeat on a schedule.

| System | How it gets in | Source kind | Default trust |
|---|---|---|---|
| Local repositories | Read in place and watched for changes. Code in Rust, Python, JavaScript, TypeScript, Go, Java and C; docs; `CODEOWNERS`; OpenAPI files; recent git history | `repo` | source of truth |
| GitHub issues, discussions and pull-request reviews | `anchor fetch David-A-Phillips/REPO` (uses your `gh` login), or a `[[github]]` entry the daemon refreshes | `issues`, `code_review` | reference only |
| Confluence | `anchor fetch-atlassian`, or an `[[atlassian]]` entry; Cloud and Data Center | `wiki` | reference only |
| Jira | `anchor fetch-atlassian`, or issue JSON you export yourself | `jira` | reference only |
| Other wikis | A directory of Markdown or HTML pages, with comment threads in sidecar files | `wiki` | reference only |
| API approval portal | A directory of API records, approvals and OpenAPI definitions | `api_portal` | source of truth |
| Organisation chart | A YAML, JSON or TOML file: portfolios, domains, components, services, teams, people | `org_model` | source of truth |

```toml
[[sources]]
id = "payments-api"
kind = "repo"
trust = "source_of_truth"
location = "/src/payments-api"
depth = "full"                 # phone_book | map | full: how much of it to hold

[[github]]                     # keep a repository's issues and reviews exported
repository = "acme/payments-api"
interval_minutes = 60

[[atlassian]]                  # keep Confluence spaces and Jira projects exported
base_url = "https://acme.atlassian.net"
deployment = "cloud"
email_env = "ATLASSIAN_EMAIL"  # names of environment variables; secrets are never stored
token_env = "ATLASSIAN_TOKEN"
confluence_spaces = ["ENG"]
jira_projects = ["PAY"]
interval_minutes = 60
```

After a fetch, add the `[[sources]]` entries the command prints. Details, file formats, depth
levels and database size are in [docs/data-sources.md](docs/data-sources.md); the Atlassian
connector has its own page, [docs/atlassian.md](docs/atlassian.md). It has been tested against a
mock of the Atlassian APIs only, not yet against a real site.

## Models and third-party APIs

No model is required. Add them when you want written explanations (`anchor query --explain`) or
tagging at ingestion.

```toml
[models.system1]               # small and fast: classification fallback, tagging at ingestion
provider = "ollama"            # ollama | anthropic | openai_compatible | disabled
model = "llama3.2:3b"

[models.system2]               # larger: written explanations, only when asked
provider = "anthropic"
model = "claude-opus-5-5"
api_key_env = "ANTHROPIC_API_KEY"

[models.routing]
allow_cloud = true
max_cloud_usd_per_day = 5.0
```

- **Local:** `anchor ollama provision` installs and starts Ollama in your data directory and
  pulls a model (Linux; elsewhere install Ollama yourself and set `base_url`).
- **Anthropic:** set `provider = "anthropic"` and the name of the variable holding your key.
- **OpenAI and compatible gateways** (Azure OpenAI, OpenRouter, vLLM, LM Studio): set
  `provider = "openai_compatible"`, `base_url` and `api_key_env`.

A cloud model receives the question and the evidence retrieved for it, nothing else.
`anchor spend` reports tokens and estimated cost per tier. Provider details, cost controls and
what each tier does are in [docs/models.md](docs/models.md). Only the Ollama provider has been
run against a real server so far.

## Using Anchor from an agent (MCP)

`anchor mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server over stdio,
the interface agent hosts such as Claude Code, Claude Desktop and Cursor speak. It opens the same
database as the CLI and the daemon, and can run alongside the daemon.

`anchor mcp install` registers the server with Claude Code, Claude Desktop and Cursor
(`--print` shows what it would change). By hand, for Claude Code:

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
| `why` | A deeper git blame: lines → commits → pull request, review decisions, tickets, pages |
| `deepen` | Ingest a shallow source more deeply; call it when `ask` lists the source under `shallow`, then ask again |
| `status` | Sources with their kind, trust and depth; entity counts |

Entities are addressed by key (`service:billing-api`, as returned by `ask` and `search`) or by
name. Tool failures come back as tool errors (`isError`), so the agent can correct itself.
Protocol revisions 2024-11-05, 2025-03-26 and 2025-06-18 are supported. For hosts that do not
speak MCP, `anchor query --json` and the dashboard's `POST /api/query` return the same data.

## Host it for a team

One machine runs the sync server; everyone else points at it.

```sh
# server
docker build -t anchor . && docker run -d -p 7891:7891 -v anchor-data:/data \
    -e ANCHOR_SYNC_TOKEN=<long random string> anchor
```

```toml
# each client's anchor.toml
mode = "client"

[sync]
server_url = "ws://anchor.internal:7891/sync"
auth_token_env = "ANCHOR_SYNC_TOKEN"
default_depth = "phone_book"   # hold other teams' sources shallowly; deepen on demand
```

```sh
anchor sync        # or run the daemon, which syncs on an interval
```

Put a TLS-terminating reverse proxy in front for anything beyond a private network; the server
itself speaks plain WebSocket and gRPC. For air-gapped sharing, `anchor export` writes a signed
`.contextpack` file that `anchor import` merges. Setup without Docker, device trust, TLS and the
current limits (no per-user access control) are in [docs/hosting.md](docs/hosting.md).

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
| `anchor why TARGET [--json]` | Why is this code the way it is: blame, then commit → pull request → review → ticket → pages |
| `anchor mcp` | Serve the Model Context Protocol on stdio, for agents (Claude Code, Cursor, …) |
| `anchor mcp install [--client …] [--print]` | Register the MCP server with Claude Code, Claude Desktop or Cursor |
| `anchor fetch-atlassian [URL]` | Export Confluence spaces and Jira projects (incremental; see `docs/atlassian.md`) |
| `anchor eval --truth FILE` | Score the engine against questions with known answers |
| `anchor compare DIR [--truth FILE] [--reindex] [--kinds]` | Score Anchor next to plain retrieval and other RAG systems: quality, latency, disk and memory |
| `anchor fetch David-A-Phillips/REPO [--issues N] [--discussions N] [--pulls N] [--out DIR]` | Export a GitHub repo's issues, discussions and PR reviews |
| `anchor deepen SOURCE [--depth map\|full]` | Ingest a source more deeply |
| `anchor export [-o FILE]` / `anchor import FILE` | Write or merge a signed `.contextpack` |
| `anchor server` / `anchor sync` | Run the central sync server / sync once against it |
| `anchor ollama status\|provision\|teardown\|rebuild` | Manage the local Ollama backend |
| `anchor demo --out DIR` | Generate a synthetic enterprise to try Anchor on |

## More documentation

| Page | Contents |
|---|---|
| [docs/install.md](docs/install.md) | apt, dnf, Homebrew, install script, Docker, Windows, running as a service |
| [RELEASING.md](RELEASING.md) | Cutting a release: packages, repositories, required secrets |
| [docs/data-sources.md](docs/data-sources.md) | Source kinds and formats, GitHub fetch, depth levels, database size |
| [docs/atlassian.md](docs/atlassian.md) | Confluence and Jira connector |
| [docs/models.md](docs/models.md) | Model tiers, providers, cost controls, managing Ollama |
| [docs/hosting.md](docs/hosting.md) | Team sync server, clients, TLS, packs |
| [docs/benchmarks.md](docs/benchmarks.md) | Comparison with LlamaIndex, LangChain and LightRAG: results, method, dataset, caveats |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Design: units, trust-weighted paths, sync, storage |
| [docs/development.md](docs/development.md) | Building, tests, performance budgets, known limitations |

## Licence

MIT. See [LICENSE](LICENSE).
