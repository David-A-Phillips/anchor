#!/usr/bin/env python3
"""Export pull requests with their review discussions from GitHub.

Writes one JSON document per pull request in the shape Anchor's code-review ingester reads
(`anchor::engine::ingest::review`):

    {
      "pull_request":    <GET /repos/{repo}/pulls/{n}>,
      "reviews":         <GET /repos/{repo}/pulls/{n}/reviews>,
      "review_comments": <GET /repos/{repo}/pulls/{n}/comments>,
      "issue_comments":  <GET /repos/{repo}/issues/{n}/comments>,
      "review_threads":  [{"id", "is_resolved", "is_outdated", "resolved_by": {"login"}, "comments": [ids]}]
    }

Requires the GitHub CLI (`gh`) to be installed and authenticated; only public data is read.
Already exported pull requests are skipped, so the script can be interrupted and resumed.

    scripts/corpus/fetch_reviews.py --repo kubernetes/kubernetes --out corpus/reviews/kubernetes --count 150
"""

import argparse
import json
import pathlib
import subprocess
import sys
import time

THREADS_QUERY = """
query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          isOutdated
          resolvedBy { login }
          comments(first: 100) { nodes { databaseId } }
        }
      }
    }
  }
}
"""

DEFAULT_QUERIES = [
    # Long, concluded discussions.
    "is:pr is:merged comments:>15",
    # Reviews that currently block a change: real answers to "why is this blocked?".
    "is:pr is:open review:changes_requested",
    # Closed without merging: where rejected and dismissed discussions live.
    "is:pr is:closed is:unmerged comments:>10",
]


def gh(args, retries=5):
    """Runs `gh api ...` and returns stdout, backing off on rate limits and transient failures."""
    delay = 5.0
    for attempt in range(retries):
        proc = subprocess.run(["gh", "api", *args], capture_output=True, text=True)
        if proc.returncode == 0:
            return proc.stdout
        message = proc.stderr.strip()
        transient = any(
            marker in message.lower()
            for marker in ("rate limit", "abuse", "timeout", "502", "503", "504", "connection")
        )
        if not transient or attempt == retries - 1:
            raise RuntimeError(f"gh api {' '.join(args[:2])} failed: {message[:300]}")
        time.sleep(delay)
        delay = min(delay * 2, 120.0)
    raise RuntimeError("unreachable")


def paginated(path):
    """Returns the concatenation of every page of a list endpoint."""
    out = gh(["--paginate", "-H", "Accept: application/vnd.github+json", f"{path}?per_page=100"])
    items, decoder, index = [], json.JSONDecoder(), 0
    while index < len(out):
        while index < len(out) and out[index].isspace():
            index += 1
        if index >= len(out):
            break
        page, index = decoder.raw_decode(out, index)
        items.extend(page if isinstance(page, list) else [page])
    return items


def review_threads(owner, name, number):
    threads, cursor = [], None
    while True:
        args = ["graphql", "-f", f"query={THREADS_QUERY}", "-f", f"owner={owner}", "-f", f"name={name}",
                "-F", f"number={number}"]
        if cursor:
            args += ["-f", f"cursor={cursor}"]
        data = json.loads(gh(args))["data"]["repository"]["pullRequest"]["reviewThreads"]
        for node in data["nodes"]:
            threads.append({
                "id": node["id"],
                "is_resolved": node["isResolved"],
                "is_outdated": node["isOutdated"],
                "resolved_by": node["resolvedBy"],
                "comments": [c["databaseId"] for c in node["comments"]["nodes"] if c.get("databaseId")],
            })
        if not data["pageInfo"]["hasNextPage"]:
            return threads
        cursor = data["pageInfo"]["endCursor"]


def search_numbers(repo, query, wanted):
    numbers, page = [], 1
    while len(numbers) < wanted and page <= 10:
        out = gh(["-X", "GET", "search/issues", "-f", f"q=repo:{repo} {query}", "-f", "sort=updated",
                  "-f", "order=desc", "-f", "per_page=100", "-f", f"page={page}"])
        items = json.loads(out).get("items", [])
        if not items:
            break
        numbers.extend(item["number"] for item in items)
        page += 1
        time.sleep(2.5)  # the search API allows 30 requests per minute
    return numbers[:wanted]


def export(repo, number, out_dir):
    target = out_dir / f"{number}.json"
    if target.exists():
        return False
    owner, name = repo.split("/")
    document = {
        "pull_request": json.loads(gh([f"repos/{repo}/pulls/{number}"])),
        "reviews": paginated(f"repos/{repo}/pulls/{number}/reviews"),
        "review_comments": paginated(f"repos/{repo}/pulls/{number}/comments"),
        "issue_comments": paginated(f"repos/{repo}/issues/{number}/comments"),
        "review_threads": review_threads(owner, name, number),
    }
    temporary = target.with_suffix(".json.part")
    temporary.write_text(json.dumps(document, ensure_ascii=False, indent=1), encoding="utf-8")
    temporary.replace(target)
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repo", required=True, help="owner/name")
    parser.add_argument("--out", required=True, type=pathlib.Path, help="output directory")
    parser.add_argument("--count", type=int, default=150, help="pull requests to export in total")
    parser.add_argument("--query", action="append", help="GitHub search qualifiers (repeatable); defaults cover "
                        "merged, blocked and rejected pull requests")
    parser.add_argument("--number", action="append", type=int, help="export this pull request (repeatable)")
    args = parser.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    numbers = list(args.number or [])
    if not numbers:
        queries = args.query or DEFAULT_QUERIES
        per_query = -(-args.count // len(queries))
        seen = set()
        for query in queries:
            for number in search_numbers(args.repo, query, per_query):
                if number not in seen:
                    seen.add(number)
                    numbers.append(number)
    exported = failed = 0
    for index, number in enumerate(numbers[: args.count] if not args.number else numbers, start=1):
        try:
            if export(args.repo, number, args.out):
                exported += 1
        except (RuntimeError, KeyError, TypeError, json.JSONDecodeError) as err:
            failed += 1
            print(f"#{number}: {err}", file=sys.stderr)
        if index % 25 == 0:
            print(f"{args.repo}: {index}/{len(numbers)} processed, {exported} new, {failed} failed", flush=True)
    print(f"{args.repo}: done, {exported} new, {failed} failed, {len(list(args.out.glob('*.json')))} on disk")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
