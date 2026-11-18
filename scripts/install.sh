#!/bin/sh
# Anchor installer: downloads a prebuilt release archive, verifies its SHA-256 and installs
# the `anchor` binary.
#
#   curl -fsSL https://raw.githubusercontent.com/David-A-Phillips/anchor/main/scripts/install.sh | sh
#
# Environment:
#   ANCHOR_REPO         GitHub "owner/name" (default: David-A-Phillips/anchor; set it for a fork)
#   ANCHOR_VERSION      version without the leading "v" (default: latest release)
#   ANCHOR_INSTALL_DIR  install directory (default: $HOME/.local/bin)
#   ANCHOR_BASE_URL     directory URL holding the archive and its .sha256 file (a mirror or a
#                       local test server); requires ANCHOR_VERSION
set -eu

ANCHOR_REPO="${ANCHOR_REPO:-David-A-Phillips/anchor}"
INSTALL_DIR="${ANCHOR_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

if have curl; then
  fetch() { curl -fsSL -o "$2" "$1"; }
elif have wget; then
  fetch() { wget -q -O "$2" "$1"; }
else
  die "need curl or wget"
fi

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux) os_part=unknown-linux-gnu ;;
  Darwin) os_part=apple-darwin ;;
  *) die "unsupported OS '$os' (Windows: download the .zip from the releases page)" ;;
esac
case "$arch" in
  x86_64 | amd64) arch_part=x86_64 ;;
  aarch64 | arm64) arch_part=aarch64 ;;
  *) die "unsupported architecture '$arch'" ;;
esac
target="${arch_part}-${os_part}"

version="${ANCHOR_VERSION:-}"
version="${version#v}"
if [ -z "$version" ]; then
  [ -z "${ANCHOR_BASE_URL:-}" ] || die "ANCHOR_BASE_URL requires ANCHOR_VERSION"
  have curl || die "resolving the latest version needs curl; set ANCHOR_VERSION instead"
  # /releases/latest redirects to /releases/tag/vX.Y.Z, but only for a full release: GitHub
  # leaves pre-releases out of it. Without a full release, take the newest release of any kind.
  final=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${ANCHOR_REPO}/releases/latest") \
    || die "cannot resolve the latest release of ${ANCHOR_REPO}"
  case "$final" in
    */releases/tag/*) version="${final##*/}" ;;
    *) version=$(curl -fsSL "https://api.github.com/repos/${ANCHOR_REPO}/releases?per_page=1" \
         | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1) \
         || die "cannot list the releases of ${ANCHOR_REPO}" ;;
  esac
  version="${version#v}"
  [ -n "$version" ] || die "cannot determine the latest version of ${ANCHOR_REPO}; set ANCHOR_VERSION"
fi

name="anchor-${version}-${target}"
archive="${name}.tar.gz"
base="${ANCHOR_BASE_URL:-https://github.com/${ANCHOR_REPO}/releases/download/v${version}}"
base="${base%/}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading ${archive} ..."
fetch "${base}/${archive}" "${tmp}/${archive}" || die "download failed: ${base}/${archive}"
fetch "${base}/${archive}.sha256" "${tmp}/${archive}.sha256" \
  || die "download failed: ${base}/${archive}.sha256"

expected=$(awk '{print $1; exit}' "${tmp}/${archive}.sha256")
[ -n "$expected" ] || die "empty checksum file"
if have sha256sum; then
  actual=$(sha256sum "${tmp}/${archive}" | awk '{print $1}')
elif have shasum; then
  actual=$(shasum -a 256 "${tmp}/${archive}" | awk '{print $1}')
else
  die "need sha256sum or shasum to verify the download"
fi
if [ "$expected" != "$actual" ]; then
  die "checksum mismatch for ${archive}: expected ${expected}, got ${actual}"
fi
say "Checksum OK."

tar -xzf "${tmp}/${archive}" -C "$tmp" || die "cannot extract ${archive}"
[ -f "${tmp}/${name}/anchor" ] || die "archive does not contain ${name}/anchor"

mkdir -p "$INSTALL_DIR"
cp "${tmp}/${name}/anchor" "${INSTALL_DIR}/anchor.new"
chmod 755 "${INSTALL_DIR}/anchor.new"
mv -f "${INSTALL_DIR}/anchor.new" "${INSTALL_DIR}/anchor"

say "Installed ${INSTALL_DIR}/anchor (version ${version}, ${target})."
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) say "Note: ${INSTALL_DIR} is not on your PATH. Add it, e.g.:  export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac
say ""
say "Next steps:"
say "  anchor setup            # guided configuration"
say "  anchor mcp install      # register Anchor with Claude Code (--client all for Desktop/Cursor)"
