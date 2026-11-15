#!/usr/bin/env bash
# Fetches the public corpus used by Anchor's real-world end-to-end tests:
# large, heavily documented, collaboratively maintained open-source projects.
#
#   scripts/fetch-corpus.sh [corpus-dir]        (default: ./corpus, which is git-ignored)
#
# Repositories are shallow clones. Pull-request review discussions are exported with the GitHub
# CLI (`gh`, authenticated); set ANCHOR_CORPUS_PRS=0 to skip them. Everything is resumable.
set -euo pipefail

root="${1:-corpus}"
prs="${ANCHOR_CORPUS_PRS:-150}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$root/repos" "$root/reviews"

clone() {
  local repo="$1" dir="$root/repos/${1##*/}"
  if [ -d "$dir/.git" ]; then
    echo "have   $repo"
  else
    echo "clone  $repo"
    git clone --quiet --depth 1 --single-branch "https://github.com/$repo.git" "$dir"
  fi
}

# Code at scale (C), MAINTAINERS ownership, a large reStructuredText documentation tree.
clone torvalds/linux
# Code at scale (Go), OWNERS files and OWNERS_ALIASES.
clone kubernetes/kubernetes
# The organisation model: SIGs, leads and subprojects (sigs.yaml).
clone kubernetes/community
# Design proposals with reviewers, approvers and status (KEPs).
clone kubernetes/enhancements
# Go and TypeScript with a large CODEOWNERS file.
clone grafana/grafana

if [ "$prs" != "0" ]; then
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    python3 "$script_dir/corpus/fetch_reviews.py" --repo kubernetes/kubernetes --out "$root/reviews/kubernetes" --count "$prs" || true
    python3 "$script_dir/corpus/fetch_reviews.py" --repo grafana/grafana --out "$root/reviews/grafana" --count "$prs" || true
  else
    echo "skip   pull-request reviews (GitHub CLI not installed or not authenticated)" >&2
  fi
fi

du -sh "$root"/repos/* "$root"/reviews/* 2>/dev/null || true
