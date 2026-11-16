# Releasing Anchor

One tag produces everything: archives, `.deb`, `.rpm`, checksums, signatures, attestations, the apt
and dnf repositories and the Homebrew tap update. Nothing here has been run on GitHub yet (the
workflow is validated with `actionlint` only); do a dry run first.

## Checklist

1. **Dry run.** Actions > Release > *Run workflow* on the branch you intend to release. It builds
   and packages all five targets plus the repositories and uploads them as workflow artifacts
   (`dist-*`, `repos-dry-run`, `release-dry-run`); it publishes nothing. Download `repos-dry-run`,
   serve it (`python3 -m http.server`) and try it as in `docs/install.md`.
2. **Bump the version** in `Cargo.toml` (`version = "X.Y.Z"`), then `cargo check` to refresh
   `Cargo.lock`. Add an entry at the top of `packaging/deb/changelog` (same Debian format,
   version `X.Y.Z-1`). Optional: bump `packaging/{aur,scoop,winget,nix}` (they are templates that
   packagers update from the release).
3. `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`, and
   `make dist` locally if you want to inspect the packages (see the
   header of `Makefile`).
4. **Tag and push**: `git tag -s vX.Y.Z -m "Anchor X.Y.Z" && git push origin vX.Y.Z`. The tag must
   equal `v` + the `Cargo.toml` version or the workflow stops at its first job.
5. Watch the run. Verify: the GitHub Release has archives, `.deb`/`.rpm`, `SHA256SUMS`
   (+ `.asc`), the Pages site lists the new version, and the tap has a commit `anchor X.Y.Z`.
6. Smoke test from a clean machine: `docs/install.md` sections 1-3.

## What the workflow does (`.github/workflows/release.yml`)

| Job | Does |
|-----|------|
| `meta` | reads the version from `Cargo.toml`, checks it matches the tag, decides publish (tag push) or dry run (manual) |
| `build` (5 targets) | `cargo auditable build --release --locked` (dependency list embedded in the binary: `cargo audit bin anchor`), tar.gz/zip + `.sha256`; on Linux also `make package-deb package-rpm` natively on amd64 and arm64 runners |
| `repos` | builds `site/apt` (`packaging/apt/build-repo.sh`) and `site/rpm` (`packaging/rpm/build-repo.sh`, `createrepo_c`), signs them, writes `anchor.sources`, `anchor.repo`, `index.html` |
| `pages` | deploys `site/` to GitHub Pages (tag pushes with `PAGES_ENABLED=true` only) |
| `release` | `SHA256SUMS` over every artifact, detached signature `SHA256SUMS.asc` when the key secret exists, `actions/attest-build-provenance`, `gh release create` |
| `homebrew` | rewrites `Formula/anchor.rb` in the tap from the `.sha256` files and pushes (skipped without the token) |

Linux binaries are glibc builds made on Ubuntu 22.04, so the minimum is **glibc 2.35**; the
packages depend on `libc6 >= 2.35` (deb) and the matching `GLIBC_*` symbol versions (rpm). A static
musl build was tried locally and does not work without a musl C toolchain (`ring` needs
`x86_64-linux-musl-gcc`, and SQLite, zstd and tree-sitter are C too). To get static binaries later,
use `cargo-zigbuild` (`--target x86_64-unknown-linux-musl`) in the `build` job and verify
that all C dependencies build.

Each release rebuilds the repositories from scratch: the apt and dnf repositories contain **only the
latest version**.

## Placeholders to fill (once)

| What | Where |
|------|-------|
| GitHub owner (`OWNER`) | `Cargo.toml` (`repository`, `homepage`, binstall/deb/rpm `url`), `packaging/**`, `docs/install.md`, `scripts/install.sh` default. Search for `OWNER` |
| License | `Cargo.toml` (`license`, `[package.metadata.deb] copyright`, `license-file`, `[package.metadata.generate-rpm] license`), `packaging/LICENSE.TODO` (replace by the real `LICENSE` at the repository root and point `license-file` at it), the Homebrew formulas, `packaging/aur/PKGBUILD`, `packaging/scoop/anchor.json`, `packaging/winget/OWNER.Anchor.yaml`, `packaging/nix/flake.nix`. Search for `TODO(owner)` |

## Secrets and variables

Set under Settings > Secrets and variables > Actions.

| Name | Kind | Needed for | Notes |
|------|------|-----------|-------|
| `REPO_GPG_PRIVATE_KEY` | secret | signing the apt/dnf repositories | ASCII-armored private key (`gpg --armor --export-secret-keys KEYID`). Without it a tag push **fails** in `repos` (it never publishes unsigned repositories); dry runs build unsigned ones |
| `REPO_GPG_PASSPHRASE` | secret | same | omit for a key without a passphrase |
| `RELEASE_GPG_PRIVATE_KEY` / `RELEASE_GPG_PASSPHRASE` | secrets | `SHA256SUMS.asc` | optional; may be the same key. No key: the checksums are unsigned |
| `HOMEBREW_TAP_TOKEN` | secret | tap update | fine-grained PAT with *Contents: write* on the tap repository. Unset: the `homebrew` job logs and does nothing |
| `PAGES_ENABLED` | variable | publishing the repositories | set to `true` after enabling Pages |
| `REPO_BASE_URL` | variable | URLs written into `anchor.sources` / `anchor.repo` | default `https://<owner>.github.io/<repo>`; set for a custom domain |
| `HOMEBREW_TAP_REPO` | variable | tap location | default `<owner>/homebrew-tap` |

One-time setup:

```sh
# Signing key (publish the public half through the repositories; keep the private half secret)
gpg --quick-generate-key "Anchor Releases <releases@example.com>" ed25519 sign 3y
gpg --armor --export-secret-keys <KEYID>      # -> REPO_GPG_PRIVATE_KEY
```

* GitHub Pages: Settings > Pages > Source: **GitHub Actions**. Then set `PAGES_ENABLED=true`.
  Pages on private repositories needs a paid plan. The release workflow's `github-pages`
  environment must allow tag refs (Settings > Environments > github-pages > deployment branches).
* Tap: create the repository `<owner>/homebrew-tap` (any initial commit); create the token.
* Attestations (`actions/attest-build-provenance`) need a public repository or GitHub Enterprise;
  the step is `continue-on-error` otherwise.

## Yanking a release

* **GitHub Release / archives:** `gh release delete vX.Y.Z --cleanup-tag` (or edit it to a
  pre-release). The install script then resolves to the previous release.
* **apt / dnf:** the repositories hold only the newest release. To roll back, re-run the `repos` and
  `pages` jobs of the previous tag's workflow run (Actions > that run > *Re-run all jobs*), or fix
  forward with a new patch version. Clients that already installed the bad version keep it until they
  upgrade.
* **Homebrew:** revert the tap commit (`git revert <sha> && git push` in the tap).
* **crates.io (if published):** `cargo yank --version X.Y.Z` (does not delete).
* A bad tag: never reuse a version number; release `X.Y.Z+1`.

## Other distribution routes (not built, not submitted)

**PPA (Launchpad) / Debian proper** need a *source* package, not the binary `.deb` from `cargo-deb`:
a `debian/` directory (`control`, `rules`, `changelog`, `copyright` in DEP-5 format, `source/format`,
`compat` through `debhelper-compat`), built with `dpkg-buildpackage -S` and uploaded with `dput`
(signed by a key registered on Launchpad). Launchpad and Debian build offline, so the Rust
dependencies must be vendored (`cargo vendor` + `.cargo/config.toml`) for a PPA; Debian proper
additionally requires that every crate is packaged in Debian as `librust-*-dev` (hundreds of
crates, many not packaged yet, e.g. candle) and a DFSG-free license, a sponsor or a Debian
Developer, an ITP bug, and a clean `lintian --pedantic`. Practically: PPA is feasible with a
vendored tarball; Debian proper is a long project. Use the apt repository above meanwhile.
The package name `anchor-context` avoids a clash with existing Debian/Fedora `anchor` packages.

**homebrew-core** requires notability (the project is accepted only if it is popular enough: roughly
75 stars / 30 forks / 30 watchers for self-submitted projects, and an age of 30 days), an
OSI-approved license, a stable tagged source tarball, building from source with no vendored
binaries (use `packaging/homebrew/anchor-source.rb`, `depends_on "rust" => :build`), tests that do
not need the network, and passing `brew audit --strict --new --online` and `brew test`. Open a PR to
`Homebrew/homebrew-core` once those hold; the name `anchor` may be taken in core, in which case use
`anchor-context`.

**crates.io:** set `publish = true` (or remove the line), choose a crate name that is free (checking
needs network access to crates.io: `anchor` is a very likely clash; `anchor-context` is a safer
candidate; change `[package] name` only together with `[[bin]] name = "anchor"`), set a real
`license`, make sure `cargo package --list` includes `build.rs` and `proto/`, then
`cargo publish`. `cargo binstall` works from the GitHub Release archives through
`[package.metadata.binstall]` once the repository slug is real, with or without crates.io.

**AUR:** copy `packaging/aur/PKGBUILD` to an AUR git repository (`ssh://aur@aur.archlinux.org/anchor-context-bin.git`),
run `updpkgsums` and `makepkg --printsrcinfo > .SRCINFO`, test in a clean chroot, push.

**Scoop:** put `packaging/scoop/anchor.json` in your own bucket repository (`scoop bucket add`),
or submit to `ScoopInstaller/Extras` once notable. **winget:** `wingetcreate new <zip url>` (or
update with `wingetcreate update`) generates the manifest in `microsoft/winget-pkgs`;
`packaging/winget/OWNER.Anchor.yaml` is the starting point. Windows binaries are not code-signed,
so SmartScreen may warn.

## Local equivalents

`make dist` (archive + `.deb` + `.rpm` + `SHA256SUMS` in `target/dist/`), `make apt-repo`,
`make rpm-repo` (needs `createrepo_c`), `packaging/gen-assets.sh`,
`packaging/test/{deb,rpm}-in-container.sh` (run inside Docker containers, see the script headers) and
`packaging/test/brew-local.sh` (local throwaway Homebrew tap).
