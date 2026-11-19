# Anchor

**One graph of your code, docs, tickets and org chart, that answers who owns it, why it is this
way, and what depends on it.**

Anchor ingests repositories, wikis, trackers, code reviews, API approvals and your organisation
model into a single local graph, and answers questions against it in milliseconds, for people
and for AI agents.

```console
$ anchor query "Who owns the payments-api service?"
payments-api is owned by Payments Platform [source of truth].

$ anchor why src/ledger/charge.rs:120          # abridged
commit 4f2a9c1e07 by Alice Smith on 2025-03-01 — 1 line(s) [fix]
  PAY-12: retry charges with a bounded backoff (#7)
  pull request pr:ledger#7 "Retry charges" [merged]
    - Review by bob: "Unbounded retries will hammer the gateway." — dismissed: capped at three
  ticket PAY-12 "Charges fail intermittently under load" [done]

$ anchor impact --diff                         # what depends on my uncommitted changes?
Affected, directly or through what depends on them: 3 services, 7 files, 12 symbols
   1 hop  service:checkout-api (service)
       via file:ledger:src/charge.rs <-imports- file:checkout-api:src/pay.rs ...
Owners to notify:
  Checkout (team): service:checkout-api, ...
```

- **Local first.** One binary and a SQLite file. No server, model or network required.
- **Trust aware.** Code and approved records outrank wikis and tickets, and conflicts are
  reported with the winner.
- **Fast.** Rules and graph traversal, about 4 ms per question; language models are optional.
- **Change aware.** `impact` shows what depends on a whole diff, or on everything a team owns,
  and who to tell, and can comment on every pull request ([CI](docs/ci.md)); `hotspots` finds the
  most depended-on services and the ones owned by nobody or only ever changed by one person.
- **Built for agents.** An MCP server and a skill for Claude Code, Cursor and others.
- **Shareable.** A team server syncs the graph; each machine holds only the depth it needs.

## Install

```sh
sudo apt-get install anchor-context      # Debian, Ubuntu
sudo dnf install anchor-context          # Fedora, RHEL
brew install David-A-Phillips/tap/anchor # macOS, Linux
cargo build --release                    # from source
```

The apt, dnf and Homebrew repositories are not published yet; until they are, take the `.deb`,
`.rpm` or archive from [Releases](https://github.com/David-A-Phillips/anchor/releases).
The Windows zip includes a Windows service. Repository setup, the install script, Docker and
what has been tested on each platform: [docs/install.md](docs/install.md).

## Quick start

Try it on a generated organisation first (24 repositories with history, a wiki, tickets, code
reviews, API approvals and an org chart):

```sh
anchor demo --out ~/anchor-demo
export ANCHOR_CONFIG=~/anchor-demo/anchor.toml     # the commands below use the demo

anchor ingest                                          # under a second
anchor query "Who is responsible for the limits-gateway service?"
anchor why identity-web:src/identity_web.c:41          # blame -> pull request -> review -> ticket
anchor impact service:limits-gateway                   # what depends on it, and who to tell
anchor hotspots                                        # most depended-on services, ownership risks
```

Then point it at your own code:

```sh
unset ANCHOR_CONFIG
anchor source add ~/src/payments-api     # writes the config, infers kind and trust, ingests
anchor query "Who owns the payments-api service?"
anchor why src/ledger/charge.rs:120
anchor impact --diff                     # run inside the repository
anchor daemon                            # keep it current; dashboard on http://localhost:7890
```

No model is needed for any of this. More: [commands](docs/commands.md) · [`anchor why`](docs/why.md) ·
[impact on pull requests](docs/ci.md)

## Data sources

| Source | How |
|---|---|
| Local repositories | `anchor source add <path>`: code, docs, CODEOWNERS, git history |
| GitHub issues, discussions, reviews | `anchor fetch OWNER/REPO` |
| Confluence and Jira | `anchor fetch-atlassian` |
| Wiki exports, API approval portals, org charts | A directory or file as a source |

Each source is `source_of_truth` or `reference_only`, and can be held at phone-book, map or
full depth. Details: [docs/data-sources.md](docs/data-sources.md) ·
[docs/atlassian.md](docs/atlassian.md)

## AI agents

```sh
anchor mcp install     # registers the MCP server and installs the skill for Claude Code
```

The skill tells the agent when to ask Anchor, how to read trust levels and conflicts, and how to
keep the index current. Other hosts and details: [docs/agents.md](docs/agents.md)

## Models

None are needed. Optionally add a local model (Ollama) for tagging at ingestion, and Anthropic or
any OpenAI-compatible API for written explanations, with a daily cost cap:
[docs/models.md](docs/models.md)

## Team hosting

Run `anchor server` (Docker image, systemd unit or Windows service) and point clients at it; the
graph syncs as signed deltas. [docs/hosting.md](docs/hosting.md)

## Benchmarks

Two kinds, both reproducible from this repository ([docs/benchmarks.md](docs/benchmarks.md) has
the method, every number and the caveats):

**Anchor's own corpus**: a generated 100-repository organisation (code, `CODEOWNERS`, an org
model, wikis, tickets, API approvals, reviews) with 735 questions whose answers are known. It
measures what Anchor is built for, and it was built by this project, so it favours Anchor.

| | Anchor | Best other (LlamaIndex hybrid) |
|---|---|---|
| Answer in the retrieved context | 99% | 89% |
| A reader model (`llama3.1:8b`) answers correctly from it, 131-question sample | 84% | 49% |
| Stale owner claim beaten by the authoritative one (12 questions) | 12 of 12 | 1 of 12 |
| Tokens per question / query time | 1,287 / 4 ms | 3,100 / 30–50 ms |

**Standard multi-hop benchmarks**: MuSiQue, 2WikiMultiHopQA and HotpotQA as released for HippoRAG 2
(1,000 questions each over 6,000 to 11,700 Wikipedia passages), scored as graph-RAG papers score
them. Not chosen by this project; three ranking constants were tuned on the first 300 questions
of each, and the other 700 score the same. Recall@5, average of the three:

| | Recall@5 |
|---|---|
| BM25 | 61.1 |
| **Anchor, no model** | **72.1** |
| HippoRAG (70B model) | 73.6 |
| **Anchor + a small local embedding model** (`nomic-embed-text`; in-process `bge-small`: 75.0) | **75.3** |
| NV-Embed-v2 (7B embedding model) | 80.2 |
| HippoRAG 2 (70B model) | 87.1 |

Without a model Anchor links pages by the titles they name and spreads relevance along those
links: on 2WikiMultiHopQA it matches HippoRAG 2 (91.0 against 90.4); it is weakest on MuSiQue
(47.8, 53.0 with embeddings), whose questions rarely use the passages' own names. A local 8B
model naming each passage's entities added almost no links beyond the titles. Published rows are
the HippoRAG 2 paper's; only BM25 was rerun here.

**Change impact** on the same synthetic corpus: `anchor impact` finds 125 of 125 API consumers and
names the right owner 129 of 129 times, in 0.3 ms per query.

## Status

Beta ([0.1.2-beta](https://github.com/David-A-Phillips/anchor/releases)). Developed and tested
on Linux x86_64; macOS and Windows builds are produced by CI but lightly exercised.
[Architecture](docs/ARCHITECTURE.md) · [Development](docs/development.md) ·
[Releasing](RELEASING.md)

## License

[MIT](LICENSE)
