# Data sources

Everything Anchor knows comes from a configured source. Each source has a kind, a location and
a trust level; `source_of_truth` sources always win a conflict against `reference_only` ones.

## What each source kind reads

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

## Confluence and Jira

`anchor fetch-atlassian` exports Confluence spaces (pages, labels, hierarchy, footer and inline
comments with their resolution) and Jira projects into the layouts the `wiki` and `jira` sources
read, incrementally and resumably, for Cloud and Data Center. Pages with read restrictions are
skipped unless you opt in. Credentials come from environment variables, never from the file.
See [`atlassian.md`](atlassian.md). The connector is tested against a mock of the
Atlassian APIs only; it has not been run against a real site yet.

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
