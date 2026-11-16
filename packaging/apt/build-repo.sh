#!/usr/bin/env bash
# Build a static, optionally signed apt repository from .deb files.
#
# Usage:  packaging/apt/build-repo.sh <out-dir> <deb>...
#
# Environment:
#   APT_SUITE      suite / codename (default: stable)
#   APT_COMPONENT  component (default: main)
#   APT_ORIGIN     Origin/Label field (default: Anchor)
#   GPG_KEY_ID     key id or fingerprint to sign with. Unset => UNSIGNED repo (local testing only;
#                  consume it with `[trusted=yes]`). The key must be in the keyring of $GNUPGHOME
#                  (the default keyring when unset). For a passphrase-protected key set
#                  GPG_PASSPHRASE.
#
# Output tree:
#   <out>/pool/<component>/a/anchor-context/*.deb
#   <out>/dists/<suite>/{Release,InRelease,Release.gpg}
#   <out>/dists/<suite>/<component>/binary-<arch>/{Packages,Packages.gz,Packages.xz,Release}
#   <out>/anchor-archive-keyring.gpg  (binary public key; only when signing)
#   <out>/anchor-archive-keyring.asc  (armored public key; only when signing)
#
# Needs dpkg-dev (dpkg-scanpackages, dpkg-deb), gzip, xz and, when signing, gpg.
set -euo pipefail

if [ "$#" -lt 2 ]; then
    sed -n '2,21p' "$0" >&2
    exit 2
fi
out=$1
shift
suite=${APT_SUITE:-stable}
component=${APT_COMPONENT:-main}
origin=${APT_ORIGIN:-Anchor}

# Resolve the .deb paths before changing directory.
debs=()
for deb in "$@"; do
    debs+=("$(cd "$(dirname "$deb")" && pwd)/$(basename "$deb")")
done

mkdir -p "$out"
out=$(cd "$out" && pwd)
pool="pool/${component}/a/anchor-context"
mkdir -p "$out/$pool"
for deb in "${debs[@]}"; do
    cp -f "$deb" "$out/$pool/"
done

arches=()
for deb in "$out/$pool"/*.deb; do
    arch=$(dpkg-deb --field "$deb" Architecture)
    case " ${arches[*]-} " in *" $arch "*) ;; *) arches+=("$arch") ;; esac
done

cd "$out"
dist="dists/${suite}"
rm -rf "$dist"
for arch in "${arches[@]}"; do
    bdir="$dist/${component}/binary-${arch}"
    mkdir -p "$bdir"
    # Run from the repo root so the Filename: fields are relative to it (pool/...).
    dpkg-scanpackages --arch "$arch" pool >"$bdir/Packages" 2>/dev/null
    gzip -9kn "$bdir/Packages"
    xz -9ek "$bdir/Packages"
    cat >"$bdir/Release" <<REL
Archive: ${suite}
Component: ${component}
Origin: ${origin}
Label: ${origin}
Architecture: ${arch}
REL
done

{
    echo "Origin: ${origin}"
    echo "Label: ${origin}"
    echo "Suite: ${suite}"
    echo "Codename: ${suite}"
    echo "Date: $(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S UTC')"
    echo "Architectures: ${arches[*]}"
    echo "Components: ${component}"
    echo "Description: Anchor package repository"
    for algo in MD5Sum:md5sum SHA256:sha256sum; do
        echo "${algo%%:*}:"
        (
            cd "$dist"
            find . -type f ! -path ./Release ! -path ./InRelease ! -path ./Release.gpg | LC_ALL=C sort |
                while read -r f; do
                    f=${f#./}
                    printf ' %s %16d %s\n' "$("${algo##*:}" "$f" | cut -d' ' -f1)" "$(stat -c %s "$f")" "$f"
                done
        )
    done
} >"$dist/Release"

if [ -n "${GPG_KEY_ID:-}" ]; then
    gpg_args=(--batch --yes --default-key "$GPG_KEY_ID")
    if [ -n "${GPG_PASSPHRASE:-}" ]; then
        gpg_args+=(--pinentry-mode loopback --passphrase "$GPG_PASSPHRASE")
    fi
    gpg "${gpg_args[@]}" --clearsign -o "$dist/InRelease" "$dist/Release"
    gpg "${gpg_args[@]}" --armor --detach-sign -o "$dist/Release.gpg" "$dist/Release"
    gpg --batch --yes --export "$GPG_KEY_ID" >anchor-archive-keyring.gpg
    gpg --batch --yes --armor --export "$GPG_KEY_ID" >anchor-archive-keyring.asc
    echo "signed repository written to $out (suite ${suite}, key ${GPG_KEY_ID})"
else
    echo "UNSIGNED repository written to $out (suite ${suite}); consume it with [trusted=yes] only"
fi
