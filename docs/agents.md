# Using Anchor from an AI agent

Anchor gives an agent what it cannot get by reading files: who owns something, why a decision
was made, what depends on what, with every fact labelled by how far to trust it. Two pieces make
that work: the **MCP server** (the tools) and the **skill** (when to use them).

## Set it up

```sh
anchor mcp install                    # Claude Code: registers the server and installs the skill
anchor mcp install --client cursor    # or claude-desktop, or all
anchor mcp install --print            # show what would change, change nothing
```

For Claude Code this runs `claude mcp add anchor -- <path to anchor> mcp` (the absolute path of
the binary you ran, so the host does not depend on its own `PATH`) and writes the skill to
`~/.claude/skills/anchor/SKILL.md` (`--scope project` puts both in the current project;
`--no-skill` skips the skill). For Claude Desktop and Cursor it adds an entry to their MCP
configuration file, keeping a backup.

Any other MCP host: point it at the command `anchor mcp` (stdio transport).

```json
{ "mcpServers": { "anchor": { "command": "anchor", "args": ["mcp"] } } }
```

Hosts without skill support still get short usage instructions from the server itself. For
fuller guidance, paste the skill into the host's rules file:

```sh
anchor mcp skill >> AGENTS.md
```

## What the skill tells the agent

The skill is [`skills/anchor/SKILL.md`](../skills/anchor/SKILL.md). In short:

- **When to call.** Ownership, "why" questions, dependencies and relationships, and what docs,
  tickets and reviews say: ask Anchor before grepping or guessing. Run `why` on code whose
  purpose is unclear before changing it, and `impact` with the files of a change to see what
  depends on them and whom to tell. Do not call it for what a file already shows.
- **How to read answers.** Prefer the direct `answer`; `source_of_truth` evidence outranks
  `reference_only`; report `conflicts`; on `shallow`, call `deepen` and ask again; treat `gaps`
  and empty results as "Anchor does not know"; on `truncated`, narrow the question.
- **Keeping it current.** Call `status` once per session. If `daemon_running` is false, call
  `ingest` before relying on answers, and again after pulling or large edits.
- **Changing sources.** Ask the user first, then use `anchor source add <path>` or
  `anchor source remove <id>`.

Edit your copy of the skill to fit your team: name your services, say which questions should
always go to Anchor, or forbid adding sources.

## Tools

| Tool | Purpose |
|------|---------|
| `ask` | A question in; the answer (when the graph holds one), evidence with trust levels and the path that led to it, resolved conflicts, gaps and shallow sources out. No model call unless `explain` is set |
| `search` | Find entities by name or alias, optionally of one kind |
| `get` | One entity: attributes, asserting sources, relationships, text |
| `path` | The most trustworthy chain of relationships between two entities |
| `why` | A deeper git blame: lines → commits → pull request, review decisions, tickets, pages |
| `impact` | Everything that depends on any of several files, symbols or entities (pass a whole change at once), nearest first with the chain that links it, and the owners to notify. A team or person stands for everything it owns |
| `hotspots` | The most depended-on services (or components, APIs, files, symbols) with their owners and committers, flagging `no_owner`, `single_person_owner`, `reference_only_owner` and `single_committer`; owners ranked by how much they own |
| `deepen` | Ingest a shallow source more deeply; call it when `ask` lists the source under `shallow`, then ask again |
| `ingest` | Rescan sources when no daemon is keeping the graph current |
| `status` | Sources with their kind, trust and depth; entity counts |

Entities are addressed by key (`service:billing-api`, as returned by `ask` and `search`) or by
name. Tool failures come back as tool errors, so the agent can correct itself. No tool calls a
language model unless `ask` is given `explain: true`.

Every tool except `deepen` and `ingest` is bounded: graph searches stop at `[query] timeout_ms`
(10 s by default) or `[graph] max_visits` nodes and say so with `"truncated": true`, and a call
still running after three times `timeout_ms` fails with a message asking for a narrower question.
An agent is never left waiting on one pathological question. Protocol revisions 2024-11-05, 2025-03-26
and 2025-06-18 are supported.

## Daemon or no daemon

| | With `anchor daemon` running | Without |
|---|---|---|
| File changes | Picked up within milliseconds | Not seen until the next ingest |
| GitHub, Confluence, Jira | Refreshed on their configured interval | Refreshed when you run `anchor fetch` / `anchor fetch-atlassian` |
| What the agent does | Nothing | Calls `ingest` at the start of a session and after large changes |

The daemon and the MCP server can run at the same time against the same database: before each
answer, the MCP server brings its graph up to date with whatever the daemon (or an `anchor
ingest` in a terminal) has written since, by replaying the store's change log. On Linux run
the daemon as a systemd user service, on macOS with `brew services`, on Windows with
`anchor-service.exe` ([install.md](install.md)).

## Without MCP

```sh
anchor query "Who owns the payments-api service?" --json
anchor why src/ledger/charge.rs:120 --json
anchor impact --diff --json          # what depends on the files changed since HEAD
anchor hotspots --json
anchor ingest --source payments-api
```

The dashboard serves the same data over HTTP on localhost: `POST /api/query`, `/api/impact` and
`/api/hotspots`.
