---
name: anchor
description: Use the Anchor knowledge graph (MCP server `anchor`) for questions about who owns code or a service, why code or a decision is the way it is, what depends on what, and what docs, tickets and reviews say. Use it before grepping or guessing at ownership, history or cross-repository relationships, and to keep its index current or change what it indexes.
---

# Anchor

Anchor is a local graph of the user's repositories, wikis, trackers, code reviews, API approvals
and org chart. It answers from that graph in milliseconds and labels every fact with how far to
trust it. Reach it through the `anchor` MCP tools; if they are not available, use the `anchor`
command line (last section).

## When to call it

Call Anchor first when the question is about something no single file states:

| The user asks | Tool |
|---|---|
| Who owns / maintains / is on call for X? Who should review this? | `ask` |
| Why is this code like this? Why was X blocked, rejected or dismissed? | `why` for code, `ask` for decisions |
| What uses X? Which team, domain or portfolio is X in? | `ask`, then `path` or `get` to follow up |
| What breaks if I change these files? Who needs to review or be told? | `impact` with every changed file |
| What are our riskiest dependencies? What has no owner, or only one person who knows it? | `hotspots` |
| What depends on everything this team owns? (reorgs, handovers) | `impact` with the team's name |
| What do the docs, runbook or tickets say about X? | `ask` |
| Is there something called X? What is its exact name? | `search` |

Before changing code whose purpose is unclear, run `why` on the file or lines
(`path`, `path:120` or `path:120-160`): it returns the commits, pull request, review decisions,
tickets and pages behind them.

Before finishing a change that touches shared code, call `impact` with all the files you changed
in one call. Mention the affected services and the owners it lists to the user, nearest first;
`hops: 1` dependents are the ones most likely to break.

Do not call it for what you can read directly: the contents of a file you already have open,
syntax, or how to write code.

## Reading the answers

- `answer` is present when the graph answers directly. Prefer it, and cite the evidence.
- Every evidence item has `trust`. `source_of_truth` (code, CODEOWNERS, approved API records,
  the org chart) outranks `reference_only` (wikis, tickets, review threads). Never let a
  reference-only claim override an authoritative one.
- `conflicts` lists disagreements with the winner already chosen. Tell the user about them.
- `shallow` means the answer rests on a partially indexed source. Call `deepen` for that source
  and ask again before relying on the answer.
- `gaps` says what is missing (`ambiguous_entity`, `unresolved_entity`, `missing_neighbor`).
  For an ambiguous name, ask again naming the service or repository.
- An empty or low-confidence result is information: say Anchor does not know, do not invent.
- `truncated: true` means a search hit its time or size budget: the result is valid but may be
  incomplete. Narrow the question (name the entity exactly, pass fewer targets) rather than
  retrying it unchanged.

Name entities exactly when you can (service slug, file path, API name, ticket id). Questions
that spell out a relationship ("who leads the team that owns the component containing X")
are followed hop by hop and answered exactly.

## Keeping it current

Call `status` once at the start of a session.

- `daemon_running: true`: the daemon watches files and refreshes sources. Do nothing.
- `daemon_running: false`: the index is as old as the last ingest. Call `ingest` once before
  relying on answers, and again after you pull, switch branches or make large edits.
  `ingest` with a `source` argument refreshes just that source and is faster.

## Changing what is indexed

Sources are part of the user's configuration, so ask before adding or removing one. Then use
the command line:

```sh
anchor source list
anchor source add /path/to/repo              # kind, id and trust are inferred; ingests it
anchor source add /path/to/wiki --kind wiki --trust reference_only
anchor source remove <id>                    # drops it from the config and from the graph
```

Suggest adding a source when the user asks about a repository or wiki that `status` does not
list. Suggest removing one that no longer exists on disk. External systems are exported first:
`anchor fetch OWNER/REPO` (GitHub issues and reviews) and `anchor fetch-atlassian`
(Confluence and Jira) print the source entries to add.

## Without the MCP tools

```sh
anchor query "Who owns the payments-api service?" --json
anchor why src/ledger/charge.rs:120 --json
anchor impact --diff --json  # what depends on the files changed since HEAD; or name files/services
anchor hotspots --json
anchor status
anchor ingest                # all sources; --source <id> for one
```
