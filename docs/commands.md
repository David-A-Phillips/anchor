# Commands

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
| `anchor enrich [--limit N] [--links-only]` | Summarise and tag pages, tickets, PRs and APIs with the local model, and link the entities they name; `--links-only` only links |
| `anchor why TARGET [--json]` | Why is this code the way it is: blame, then commit → pull request → review → ticket → pages |
| `anchor impact TARGET… [--diff [REV]] [--kind KIND]… [--limit N] [--json \| --markdown]` | What depends on any of these files, symbols, services, APIs, teams or people, and who owns what is affected. A team or person stands for everything it owns; a component, domain or portfolio for what it contains. `--diff`: the files changed since `REV` (default `HEAD`); `--markdown`: a pull-request comment ([ci.md](ci.md)) |
| `anchor hotspots [--kind KIND] [--limit N] [--min-dependents N] [--json]` | The most depended-on services (or `component`, `api_spec`, `file`, `symbol`), their owners, who changed them (from git history) and the risks: no owner, a single-person owner, a reference-only owner, a single committer |
| `anchor source list\|add PATH\|remove ID` | List, add or remove sources (add infers kind and trust, then ingests) |
| `anchor mcp` | Serve the Model Context Protocol on stdio, for agents (Claude Code, Cursor, …) |
| `anchor mcp install [--client …] [--print]` | Register the MCP server with Claude Code, Claude Desktop or Cursor, and install the agent skill |
| `anchor mcp uninstall [--client …] [--print]` | Remove the registration and the skill again |
| `anchor mcp skill` | Print the agent skill, for hosts without skill support |
| `anchor fetch-atlassian [URL]` | Export Confluence spaces and Jira projects (incremental; see `docs/atlassian.md`) |
| `anchor eval --truth FILE` | Score the engine against questions with known answers |
| `anchor compare DIR [--truth FILE] [--reindex] [--kinds]` | Score Anchor next to plain retrieval and other RAG systems: quality, latency, disk and memory |
| `anchor fetch OWNER/REPO [--issues N] [--discussions N] [--pulls N] [--out DIR]` | Export a GitHub repo's issues, discussions and PR reviews |
| `anchor deepen SOURCE [--depth map\|full]` | Ingest a source more deeply |
| `anchor export [-o FILE]` / `anchor import FILE` | Write or merge a signed `.contextpack` |
| `anchor server` / `anchor sync` | Run the central sync server / sync once against it |
| `anchor ollama status\|provision\|teardown\|rebuild` | Manage the local Ollama backend |
| `anchor demo --out DIR [--scale tiny\|small\|medium] [--seed N] [--no-history]` | Generate a synthetic organisation to try Anchor on (repositories get a git history unless `--no-history`) |

## Dashboard

`anchor daemon` serves `http://localhost:7890` with: an overview (counts, sources and trust
levels, change-to-graph latency), a graph explorer (search, node detail with provenance, trust-
weighted path finder), a query tester (answer, intent, routing, context, conflicts, gaps), the
backlog, token spend, and snapshot export. It is unauthenticated, binds to loopback and refuses
requests addressed to other host names.

JSON API: `/api/health`, `/api/stats`, `/api/graph/search?q=`, `/api/graph/node/{id}`,
`/api/graph/path?from=&to=`, `/api/graph/hierarchy`, `POST /api/query`,
`/api/impact?targets=a,b&kinds=&limit=`, `/api/hotspots?kind=&limit=&min_dependents=`, `/api/backlog`,
`POST /api/backlog/{id}/{resolve|dismiss|reopen|retry}`, `/api/spend?days=`,
`POST /api/sources/{id}/rescan`, `POST /api/export`.
