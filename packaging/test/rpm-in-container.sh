#!/bin/sh
# Container-side check of the rpm path:
#   docker run --rm -v <dir-with-rpm>:/in:ro -v <packaging-dir>:/pkg:ro fedora:latest /pkg/test/rpm-in-container.sh
set -eu
dnf -q -y install createrepo_c >/dev/null
mkdir -p /repo
/pkg/rpm/build-repo.sh /repo /in/*.rpm
cat >/etc/yum.repos.d/anchor.repo <<'EOF'
[anchor]
name=Anchor
baseurl=file:///repo
enabled=1
gpgcheck=0
repo_gpgcheck=0
EOF
echo "--- direct install of the file"
dnf -y --setopt=tsflags= install /in/anchor*.rpm 2>&1 | tail -6
anchor --version
anchor --help | head -3
echo "--- package metadata"
rpm -qi anchor-context | grep -E "^(Name|Version|Release|Architecture|License|Summary)"
rpm -q --requires anchor-context | head
rpm -q --recommends anchor-context 2>/dev/null || true
echo "--- files"
rpm -ql anchor-context | grep -E "man|completions|site-functions|systemd|/etc"
rpm -qc anchor-context
man -w anchor 2>/dev/null || ls /usr/share/man/man1/anchor.1*
echo "--- account"
getent passwd anchor
cd /tmp
anchor demo --out /tmp/demo --scale tiny >/dev/null && echo demo-ok
echo "--- remove and install again from the repo"
dnf -y remove anchor-context 2>&1 | tail -2
dnf -y --setopt=tsflags= install anchor-context 2>&1 | tail -2
dnf -y remove anchor-context 2>&1 | tail -1
ls /etc/anchor /var/lib/anchor 2>&1 | head -3
getent passwd anchor || echo "account removed"
rpm -q anchor-context || true
