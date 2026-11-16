#!/bin/sh
# Container-side check of the apt path. Run by `make test-apt` (see the Makefile):
#   docker run --rm -v <repo-dir>:/repo -v <this-script>:/t.sh:ro debian:bookworm-slim /t.sh trusted|signed
set -eu
export DEBIAN_FRONTEND=noninteractive
MODE=${1:-trusted}
# Container images drop man pages and docs via dpkg path-exclude; a real system keeps them.
rm -f /etc/dpkg/dpkg.cfg.d/docker /etc/dpkg/dpkg.cfg.d/excludes
apt-get update -qq >/dev/null
apt-get install -y -qq ca-certificates >/dev/null
chmod -R a+rX /repo
if [ "$MODE" = trusted ]; then
    echo 'deb [trusted=yes] file:/repo stable main' >/etc/apt/sources.list.d/anchor.list
else
    mkdir -p /etc/apt/keyrings
    cp /repo/anchor-archive-keyring.gpg /etc/apt/keyrings/anchor.gpg
    cat >/etc/apt/sources.list.d/anchor.sources <<S
Types: deb
URIs: file:/repo
Suites: stable
Components: main
Signed-By: /etc/apt/keyrings/anchor.gpg
S
fi
apt-get update 2>&1 | grep -i -E "anchor|repo|warn|err" || true
apt-get install -y anchor-context 2>&1 | tail -6
anchor --version
anchor --help | head -4
echo "--- installed files"
dpkg -L anchor-context | grep -E "man|completions|systemd|/etc"
zcat /usr/share/man/man1/anchor.1.gz | head -3
echo "--- conffile"
dpkg-query -W -f='${Conffiles}\n' anchor-context
echo "--- account and services"
getent passwd anchor
if command -v systemd-analyze >/dev/null; then
    systemd-analyze verify /usr/lib/systemd/system/anchor-server.service || true
fi
echo "--- recommends pulled in: $(command -v git || echo no)"
cd /tmp
anchor demo --out /tmp/demo --scale tiny >/dev/null && echo demo-ok
echo "--- remove"
apt-get remove -y anchor-context 2>&1 | tail -2
ls /usr/bin/anchor 2>&1 || true
ls /etc/anchor
echo "--- purge"
apt-get purge -y anchor-context 2>&1 | tail -2
ls /etc/anchor /var/lib/anchor 2>&1 | head -3
getent passwd anchor || echo "account removed"
dpkg -l | grep anchor || echo "package gone"
