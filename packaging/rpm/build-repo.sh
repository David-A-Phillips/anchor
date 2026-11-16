#!/usr/bin/env bash
# Build a static dnf/yum repository from .rpm files with createrepo_c, optionally signing the
# repository metadata.
#
# Usage: packaging/rpm/build-repo.sh <out-dir> <rpm>...
#
# Environment:
#   GPG_KEY_ID    key to sign repodata/repomd.xml with (writes repomd.xml.asc) and to export as
#                 <out>/RPM-GPG-KEY-anchor. Unset => unsigned (local testing; use repo_gpgcheck=0).
#   GPG_PASSPHRASE  passphrase for a protected key.
#
# Output: <out>/<arch>/*.rpm  and <out>/repodata/  (one repo covering all architectures; dnf only
# looks at packages for its own arch, so x86_64 and aarch64 can share it).
set -euo pipefail

if [ "$#" -lt 2 ]; then
    sed -n '2,13p' "$0" >&2
    exit 2
fi
out=$1
shift
command -v createrepo_c >/dev/null || { echo "createrepo_c not found" >&2; exit 1; }

mkdir -p "$out"
for rpm in "$@"; do
    arch=$(basename "$rpm" .rpm)
    arch=${arch##*.}
    mkdir -p "$out/$arch"
    cp -f "$rpm" "$out/$arch/"
done
createrepo_c --general-compress-type gz "$out"

if [ -n "${GPG_KEY_ID:-}" ]; then
    gpg_args=(--batch --yes --default-key "$GPG_KEY_ID")
    if [ -n "${GPG_PASSPHRASE:-}" ]; then
        gpg_args+=(--pinentry-mode loopback --passphrase "$GPG_PASSPHRASE")
    fi
    gpg "${gpg_args[@]}" --armor --detach-sign -o "$out/repodata/repomd.xml.asc" "$out/repodata/repomd.xml"
    gpg --batch --yes --armor --export "$GPG_KEY_ID" >"$out/RPM-GPG-KEY-anchor"
    echo "signed rpm repository written to $out"
else
    echo "UNSIGNED rpm repository written to $out"
fi
