#!/usr/bin/env bash
# Rewrite version, URLs and SHA-256 checksums of the Homebrew formula from a release's .sha256
# files.
#
# Usage: packaging/homebrew/update-formula.sh <version> <dir-with-.sha256-files> [formula.rb]
#   version  without the leading "v", e.g. 0.2.0
#   dir      holds anchor-<version>-<target>.tar.gz.sha256 for the four brew targets
#   formula  defaults to packaging/homebrew/anchor.rb next to this script
#
# Environment:
#   ANCHOR_REPO      GitHub owner/name for the download URLs (default David-A-Phillips/anchor)
#   ANCHOR_BASE_URL  directory URL holding the archives, overriding the GitHub release URL
#                    (used for local testing)
set -euo pipefail

if [ "$#" -lt 2 ]; then
    sed -n '2,13p' "$0" >&2
    exit 2
fi
version=$1
dir=$2
formula=${3:-"$(dirname "$0")/anchor.rb"}
repo=${ANCHOR_REPO:-David-A-Phillips/anchor}
base=${ANCHOR_BASE_URL:-"https://github.com/${repo}/releases/download/v${version}"}

targets="aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu"
sums=""
for target in $targets; do
    file="${dir}/anchor-${version}-${target}.tar.gz.sha256"
    [ -f "$file" ] || { echo "missing $file" >&2; exit 1; }
    sum=$(awk '{print $1; exit}' "$file")
    case $sum in
        *[!0-9a-f]* | "") echo "bad checksum in $file" >&2; exit 1 ;;
    esac
    [ "${#sum}" -eq 64 ] || { echo "bad checksum length in $file" >&2; exit 1; }
    sums="${sums}${target}=${sum} "
done

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
awk -v version="$version" -v base="$base" -v sums="$sums" -v repo="$repo" '
BEGIN {
    n = split(sums, pairs, " ")
    for (i = 1; i <= n; i++) { split(pairs[i], kv, "="); sum[kv[1]] = kv[2] }
}
/^  version "/ { print "  version \"" version "\""; next }
/^  homepage "/ { print "  homepage \"https://github.com/" repo "\""; next }
/^ +url ".*\.tar\.gz"/ {
    target = ""
    for (t in sum) if (index($0, t ".tar.gz") > 0) target = t
    if (target == "") { print; next }
    match($0, /^ +/)
    print substr($0, 1, RLENGTH) "url \"" base "/anchor-" version "-" target ".tar.gz\""
    pending = target
    next
}
pending != "" && /^ +sha256 "/ {
    match($0, /^ +/)
    print substr($0, 1, RLENGTH) "sha256 \"" sum[pending] "\""
    pending = ""
    next
}
{ print }
' "$formula" >"$tmp"
cat "$tmp" >"$formula"
echo "updated $formula to ${version}"
