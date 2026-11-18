# Change impact on pull requests

`anchor impact --diff <base>` takes the files a branch changed and reports what depends on them
and who owns what is affected. With `--markdown` the report is ready to post on the pull request,
so reviewers and owners see the impact of a change before it is merged.

## GitHub Actions

Copy [`examples/github/anchor-impact.yml`](../examples/github/anchor-impact.yml) to
`.github/workflows/anchor-impact.yml`. On every pull request it:

1. checks out the repository with full history (the diff needs the base branch);
2. installs the latest Anchor release;
3. creates a throwaway configuration, adds the checkout as a source and ingests it;
4. runs `anchor impact --diff origin/<base> --markdown`, adds the report to the job summary, and
   posts it as a comment, updating the same comment on later pushes.

Pull requests from forks get a read-only token, so for them the report is only in the job
summary. It needs Anchor 0.1.2 or later (`--markdown`).

The comment looks like this:

> ### Change impact
>
> 1 target changed; affected, directly or through what depends on them: **1 API spec, 3 services**.
>
> **Owners to notify**
>
> - Cards Integrations (team): `file:recon-engine:src/lib.rs`, `service:recon-engine`, `api:recon-engine`
> - Data Core (team): `service:identity-sync`
>
> <details><summary>Nearest 4 of 4 dependents</summary> ... </details>

## What it can see

The workflow indexes only the repository it runs in, so it knows that repository's code,
`CODEOWNERS` and API definitions. Dependents in other repositories, and owners from an org
model, wiki or tracker, appear only if the job can read those too. Two ways to give it more:

- **Add more sources** before `anchor ingest`: check out the repositories your service's
  consumers live in (`actions/checkout` with `repository:` and `path:`) and `anchor source add`
  each one, or commit an `anchor.toml` that lists them and point `ANCHOR_CONFIG` at it.
- **Pull from a team server.** If your team runs `anchor server` ([hosting.md](hosting.md)), give
  the job a client configuration (`mode = "client"`, `server_url`, and the token from a secret)
  and run `anchor sync` instead of indexing from scratch.

## Other CI systems

The commands are the same anywhere with git and a shell:

```sh
export ANCHOR_CONFIG="$(mktemp -d)/anchor.toml"
anchor init
anchor source add .
anchor ingest
anchor impact --diff origin/main --markdown > impact.md   # or --json for your own formatting
```

`anchor impact --diff` exits with status 0 whether or not anything is affected; with `--json`,
`total` is the number of dependents found, so a job can fail or warn on large changes:

```sh
anchor impact --diff origin/main --json | jq -e '.total < 50'
```
