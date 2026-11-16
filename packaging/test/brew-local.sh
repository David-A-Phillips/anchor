#!/usr/bin/env bash
# Exercise the Homebrew formula against a locally served release archive, using a throwaway tap
# that is removed afterwards. Needs Homebrew and a built release archive + .sha256 files for the
# host target in <dir> (see `make archive`); other targets' checksums may be dummies.
#
# Usage: packaging/test/brew-local.sh <version> <dir-with-archives>
set -euo pipefail
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_FROM_API=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1
version=$1
dir=$(cd "$2" && pwd)
root=$(cd "$(dirname "$0")/../.." && pwd)
port=${PORT:-18765}
tap=local/anchortest

(cd "$dir" && exec python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null 2>&1) &
server=$!
cleanup() {
    kill "$server" 2>/dev/null || true
    brew uninstall --force anchor >/dev/null 2>&1 || true
    brew untap "$tap" >/dev/null 2>&1 || true
}
trap cleanup EXIT
sleep 1

brew tap-new --no-git "$tap"
formula="$(brew --repository "$tap")/Formula/anchor.rb"
# The license placeholder is not a valid SPDX id; use MIT in this throwaway copy only.
sed "s/TODO(owner)/MIT/" "$root/packaging/homebrew/anchor.rb" >"$formula"
ANCHOR_BASE_URL="http://127.0.0.1:${port}" "$root/packaging/homebrew/update-formula.sh" "$version" "$dir" "$formula"

echo "=== audit"
brew audit --strict --formula "$tap/anchor" || echo "(audit reported problems above)"
echo "=== install"
brew install --formula "$tap/anchor"
echo "=== test"
brew test "$tap/anchor"
echo "=== installed files"
brew list --verbose anchor | grep -E "bin|man|completion"
anchor --version
echo "=== service definition"
brew services info anchor 2>&1 | head -5 || true
echo "=== uninstall"
brew uninstall anchor
