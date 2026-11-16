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
```

- **Local first.** One binary and a SQLite file. No server, model or network required.
- **Trust aware.** Code and approved records outrank wikis and tickets, and conflicts are
  reported with the winner.
- **Fast.** Rules and graph traversal, about 6 ms per question; language models are optional.
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

```sh
anchor demo --out /tmp/demo && anchor --config /tmp/demo/anchor.toml ingest   # try it on sample data

anchor source add ~/src/payments-api     # add your own repository and ingest it
anchor query "Who owns the payments-api service?"
anchor why src/ledger/charge.rs:120
anchor daemon                            # keep it current; dashboard on http://localhost:7890
```

More: [commands](docs/commands.md) · [`anchor why`](docs/why.md)

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

On 735 questions over a 100-repository synthetic organisation, a reader model answered 85%
correctly from Anchor's context, against 49% from the best LlamaIndex configuration, using less
than half the tokens. The corpus is synthetic and favours the questions Anchor targets: read the
method and caveats in [docs/benchmarks.md](docs/benchmarks.md).

## Status

Beta ([0.1.1-beta](https://github.com/David-A-Phillips/anchor/releases)). Developed and tested
on Linux x86_64; macOS and Windows builds are produced by CI but lightly exercised.
[Architecture](docs/ARCHITECTURE.md) · [Development](docs/development.md) ·
[Releasing](RELEASING.md)

## License

[MIT](LICENSE)
