# Atlassian connector (Confluence and Jira)

`anchor fetch-atlassian` exports live Confluence pages and Jira issues into the on-disk layouts
that Anchor's `wiki` and `jira` sources already read. The ingesters are unchanged: the connector
only produces files, and you point ordinary `[[sources]]` at them.

> **Verification status.** The connector was written without access to an Atlassian instance. The
> endpoint paths, query parameters and JSON fields below come from knowledge of the public REST
> APIs and are exercised only against a local mock that implements the same assumptions. Expect to
> check the list in "Endpoints and fields relied on" against a real site before trusting it.

## Setup

1. Create a credential.
   * Cloud: an API token at <https://id.atlassian.com/manage-profile/security/api-tokens>. The
     account needs read access to the spaces and projects you export.
   * Data Center: a personal access token (profile, Personal Access Tokens). Read access suffices.
2. Put the secrets in environment variables (never in `anchor.toml`):
   `export ATLASSIAN_EMAIL=me@acme.com ATLASSIAN_TOKEN=...` (Cloud),
   `export ATLASSIAN_TOKEN=...` (Data Center).
3. Run a first export and add the printed sources:

```sh
anchor fetch-atlassian https://acme.atlassian.net --email-env ATLASSIAN_EMAIL \
    --token-env ATLASSIAN_TOKEN --space ENG --project PAY
```

or configure it so the daemon keeps it fresh and plain `anchor fetch-atlassian` refreshes it:

```toml
[[atlassian]]
base_url = "https://acme.atlassian.net"   # Data Center: https://wiki.example.com[/context]
deployment = "cloud"                      # or "data_center"
email_env = "ATLASSIAN_EMAIL"             # Cloud only
token_env = "ATLASSIAN_TOKEN"
confluence_spaces = ["ENG", "OPS"]        # "*" = every global space the account sees
jira_projects = ["PAY"]
# jira_jql = "labels = payments"          # alone: selects issues; with projects: narrows each
interval_minutes = 60                     # 0 = only on demand
include_restricted = false
max_pages = 0                             # 0 = unlimited, per run
max_issues = 0

[[sources]]
id = "acme-wiki"
kind = "wiki"
location = "<data dir>/atlassian/acme.atlassian.net/confluence"

[[sources]]
id = "acme-jira"
kind = "jira"
location = "<data dir>/atlassian/acme.atlassian.net/jira"
```

Data Center sites with Confluence and Jira on different hosts need one `[[atlassian]]` entry per
host (each with its own `base_url`, and only spaces or only projects).

## Config reference

| Key | Meaning |
|-----|---------|
| `base_url` | Site URL. `https` is required except for loopback hosts. |
| `deployment` | `cloud` (basic auth e-mail + API token) or `data_center` (bearer personal access token). |
| `email_env` | Cloud: name of the variable holding the e-mail. |
| `token_env` | Name of the variable holding the token. An unset or empty variable is a clear error. |
| `confluence_spaces` | Space keys; `"*"` lists global spaces (personal spaces are not included). Empty: no Confluence. |
| `jira_projects`, `jira_jql` | Projects and/or extra JQL (`ORDER BY` is stripped). Both: `project = KEY AND (jql)`. |
| `out` | Default `<data dir>/atlassian/<host>`. |
| `interval_minutes` | Daemon refresh period (minimum 5 when above 0). After a refresh that changed files the sources under `out` are re-scanned. |
| `include_restricted` | See below. |
| `max_pages`, `max_issues` | Items downloaded per run; the next run continues. |

CLI flags mirror these: `--deployment --email-env --token-env --space --project --jql --out
--include-restricted --max-pages --max-issues`. Without a URL every `[[atlassian]]` entry is
refreshed.

## What is exported

```
<out>/confluence/<SPACE>/<page id>.html            page with <meta> metadata
<out>/confluence/<SPACE>/<page id>.comments.json   threads (only when the page has comments)
<out>/jira/<PROJECT>/<KEY>.json                    one issue, REST shape plus "names"
<out>/.atlassian-state.json                        incremental state (hidden; not ingested)
```

Pages: `page-id`, `title`, `parent-id`, `parent-title`, `labels`, `author` (`Name <email>` when the
email is visible), `created`, `updated`, `url`, `space`, `version`, and `restricted_to` when
applicable. The body is the storage-format XHTML made plain: user mentions become `@<account id>`,
page links their title, code macros `<pre><code>`, status lozenges `[STATUS: x]`, info/note/warning/
tip/panel macros block quotes, task lists `[ ]`/`[x]`, Jira macros the issue key, images
`[image: file]`; table of contents, children lists and similar macros are dropped.

Comments: footer comments become `page` threads, inline comments `inline` threads with the anchor
text (`originalSelection`) and status `open`/`resolved`/`dismissed` taken from the resolution
status (anything else maps to `open`; Confluence itself normally only has open/resolved, `dismissed`
is passed through if a server reports it). Replies join the thread of their root comment and carry
`reply_to`. Resolved threads record `closed_by` and `closed_at`.

Jira: the issue exactly as returned by search with `fields=*all`, with `names` merged in (custom
field display names), so descriptions in Atlassian Document Format, links and comments work as the
Jira ingester documents. Comments missing from the search result are fetched and completed.

## Permissions: why restricted pages are skipped

A page with read restrictions is **skipped by default** and counted in the report
(`restricted pages skipped`). Anchor exports packs and syncs graphs to other machines; content that
the account can read but others may not would silently become readable there. Restrictions are also
inherited in Confluence, so a child of a restricted page is treated as restricted. The connector
lists every page with its direct restrictions on each run and applies this to ancestors it knows.

With `include_restricted = true` (or `--include-restricted`) restricted pages are exported and the
users and groups are written as `<meta name="restricted_to" content='["group:hr","user:Carol"]'>`
(JSON array, inherited entries merged in) so later features can filter. The ingesters do not
filter on it yet. A page that becomes restricted after being exported is removed from the export on
the next run (when `include_restricted` is false). Switching `include_restricted` on re-lists
everything once.

Jira has no equivalent handling: whatever the account can see through the configured projects/JQL
is exported (issue security levels are not inspected).

## Incremental, resumable runs

* Items are listed oldest-modified first (`ORDER BY lastmodified ASC` / `updated ASC`). The state
  file records per space/project the high-water mark (latest exported `updated`) and, for every
  exported item, its version/`updated`. The mark moves after each item is written, so it also is
  the resume point of an interrupted run; no separate cursor is needed.
* A new run queries `lastmodified >= "<mark minus 1 day>"` / `updated >= "<mark minus 1 day>"`
  (CQL/JQL use minute precision in the user's time zone; the margin covers every zone). Listed
  items whose `updated`/version equal the recorded ones are not downloaded again. Issues are first
  listed with `fields=updated` only and then downloaded in batches of 50.
* Deleted pages: every run also enumerates all page ids of each space, and removes exported files
  of pages that no longer exist (or became restricted). This is reliable only because the
  enumeration completed; any error aborts before anything is removed.
* Deleted, moved or no-longer-matching Jira issues are **not** removed: search cannot tell
  deletion from "no longer matches the JQL" or "lost permission". Delete files by hand or remove
  the export directory to rebuild.
* Pages moved between spaces leave the old copy until the space is re-exported.

## Robustness

HTTP 429 waits for `Retry-After` (seconds; capped at 60 s; otherwise 1, 2, 4, ... s) up to 6
retries. 5xx answers and transport failures are retried 3 times (1, 2, 4 s). Per-request timeout 30
s. 401 and 403 errors name the environment variables to check. Secrets are sent only in the
`Authorization` header and never appear in logs or errors. The sleeper is injectable for tests.

## Endpoints and fields relied on

One API family per deployment. Confluence uses the **v1 content API** on both (Cloud prefix
`/wiki/rest/api`, Data Center `/rest/api`), because it offers CQL, restrictions and comment
extensions in one place. Jira uses **v3 `search/jql`** on Cloud and **v2 `search`** on Data Center.

* `GET {c}/space?type=global&limit&start`: `results[].key` (only for `"*"`).
* `GET {c}/content/search?cql&expand&limit&start`: `results[].id`, `.title`, `.version.number`,
  `.version.when`, `.ancestors[].id`, `.restrictions.read.restrictions.user.results[]`
  (`displayName`/`publicName`/`username`/`accountId`), `.group.results[].name`, `_links.next`.
  CQL: `space = "KEY" AND type = page [AND lastmodified >= "yyyy-MM-dd HH:mm"] ORDER BY
  lastmodified ASC`. Expanding restrictions in search results is an assumption.
* `GET {c}/content/{id}?expand=body.storage,version,history,ancestors,space,metadata.labels,
  restrictions.read.restrictions.user,restrictions.read.restrictions.group`: `body.storage.value`,
  `history.createdBy.{displayName,email}`, `history.createdDate`, `version.{number,when}`,
  `ancestors[].{id,title}` (last = parent), `space.key`, `metadata.labels.results[].name`,
  `_links.base` + `_links.webui`.
* `GET {c}/content/{id}/child/comment?expand=body.storage,history,ancestors,
  extensions.inlineProperties,extensions.resolution&depth=all&limit&start`: per comment `id`,
  `body.storage.value`, `history.createdBy`, `history.createdDate`, `ancestors[]` (parent comments),
  `extensions.location` (`inline`/`footer`), `extensions.inlineProperties.originalSelection`,
  `extensions.resolution.{status,lastModifier,lastModifiedAt}`.
* Cloud Jira: `GET /rest/api/3/search/jql?jql&fields&expand=names&maxResults&nextPageToken`:
  `issues[]`, `names`, `nextPageToken`, `isLast`.
* Data Center Jira: `GET /rest/api/2/search?jql&fields&expand=names&maxResults&startAt`: `issues[]`,
  `names`, `total`.
* `GET {j}/issue/{KEY}/comment?startAt&maxResults`: `comments[]`, `total`; and `fields.comment.total`
  in search results. JQL: `project = "KEY" AND (extra)`, `updated >= "..."`, `key in ("A","B")`,
  `ORDER BY updated ASC`; `fields.updated`.

## Not covered

Attachments and their text; page history (only the current version); blog posts, databases,
whiteboards and other non-`page` content; personal spaces under `"*"`; resolving mention account
ids to names; Jira changelogs, worklogs, attachments and issue security levels; Jira deletions
(above); Confluence comment attachments; Confluence v2 API; separate Confluence/Jira hosts in one
entry. Space/project scope changes do not delete earlier exports.
