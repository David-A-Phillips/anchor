# Why is this code the way it is (`anchor why`)

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
