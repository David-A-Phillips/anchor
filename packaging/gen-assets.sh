#!/usr/bin/env bash
# Generate shell completions and the man page from a built binary into target/assets/.
# Usage: packaging/gen-assets.sh [path-to-anchor-binary]
set -euo pipefail
cd "$(dirname "$0")/.."
bin="${1:-target/release/anchor}"
out=target/assets
mkdir -p "$out"
"$bin" completions bash > "$out/anchor.bash"
"$bin" completions zsh > "$out/_anchor"
"$bin" completions fish > "$out/anchor.fish"
"$bin" man > "$out/anchor.1"
echo "assets written to $out"
