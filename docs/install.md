# Installing and sharing Anchor

Pick the first channel that fits your system. Every command below uses the placeholder
repository `David-A-Phillips/anchor`; replace `OWNER` with the real GitHub account (and, for the apt and dnf
repositories, use the real base URL: `https://David-A-Phillips.github.io/anchor` is the default GitHub Pages
address). The OS package is called **`anchor-context`** because Debian and Fedora already have
unrelated packages called `anchor`; the command it installs is still `anchor`.

| Channel | Platforms | Status |
|---------|-----------|--------|
| [1. apt](#1-debian-and-ubuntu-apt) | Debian, Ubuntu (amd64, arm64) | amd64 tested, arm64 untested |
| [2. dnf](#2-fedora-rhel-and-derivatives-dnf) | Fedora, RHEL-family (x86_64, aarch64) | x86_64 tested on Fedora, aarch64 untested |
| [3. Homebrew](#3-homebrew-macos-and-linux) | macOS, Linux | Linux x86_64 tested, macOS untested |
| [4. Install script](#4-install-script) | Linux, macOS | Linux x86_64 tested |
| [5. Docker](#5-team-server-in-docker) | any (team server) | tested |
| [6. Cargo](#6-cargo-and-from-source) | any with Rust | from source tested; `cargo binstall` untested |
| [7. Windows](#7-windows) | Windows x86_64 | untested |

A managed Ollama install (`anchor setup`) is Linux-only; elsewhere install Ollama yourself and
point `ollama.base_url` at it. The `git` command-line tool is an optional runtime dependency
(history and blame) and `gh` is optional (GitHub fetch); the packages recommend `git`.

## 1. Debian and Ubuntu (apt)

```sh
# 1. Trust the repository's signing key (a keyring only this one source uses).
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://David-A-Phillips.github.io/anchor/apt/anchor-archive-keyring.gpg \
  | sudo tee /etc/apt/keyrings/anchor.gpg >/dev/null

# 2. Add the source (deb822 format).
sudo tee /etc/apt/sources.list.d/anchor.sources >/dev/null <<'EOF'
Types: deb
URIs: https://David-A-Phillips.github.io/anchor/apt
Suites: stable
Components: main
Signed-By: /etc/apt/keyrings/anchor.gpg
EOF

# 3. Install.
sudo apt-get update
sudo apt-get install anchor-context
```

(`anchor.sources` is also published next to the repository: `curl -fsSL
https://David-A-Phillips.github.io/anchor/anchor.sources | sudo tee /etc/apt/sources.list.d/anchor.sources`.)

Or install a single file from a GitHub Release without adding a repository:

```sh
sudo apt-get install ./anchor-context_<version>-1_amd64.deb
```

- **Upgrade:** `sudo apt-get update && sudo apt-get install --only-upgrade anchor-context`
  (a plain `apt upgrade` also picks it up).
- **Uninstall:** `sudo apt-get remove anchor-context`; `sudo apt-get purge anchor-context` also
  removes `/etc/anchor`, the `anchor` system account and `/var/lib/anchor` (the sync server's data).
- Installed: `/usr/bin/anchor`, a man page (`man anchor`), bash/zsh/fish completions, documentation
  in `/usr/share/doc/anchor-context/`, two systemd units (below) and `/etc/anchor/anchor.toml`
  (a conffile: your edits survive upgrades). Nothing is enabled or started automatically.
- The binary needs glibc 2.35 or newer (Debian 12 or newer, Ubuntu 22.04 or newer).

### Running as a service

**Your own daemon** (watches your sources, serves the dashboard on <http://localhost:7890>) is a
systemd *user* service:

```sh
anchor init                       # or `anchor setup`: writes ~/.config/anchor/anchor.toml; edit it
systemctl --user enable --now anchor.service
journalctl --user -u anchor.service -f
```

The unit refuses to start until that config file exists. To keep it running after you log out:
`loginctl enable-linger "$USER"`.

**Team sync server** (`anchor server`) is a hardened *system* service running as the dedicated
`anchor` user with state in `/var/lib/anchor` (`NoNewPrivileges`, `ProtectSystem=strict`, no
capabilities, ...):

```sh
sudoedit /etc/anchor/anchor.toml                       # review sync.listen (default 127.0.0.1:7891)
echo "ANCHOR_SYNC_TOKEN=$(openssl rand -hex 32)" | sudo tee /etc/anchor/server.env
sudo chgrp anchor /etc/anchor/server.env && sudo chmod 640 /etc/anchor/server.env
sudo systemctl enable --now anchor-server.service
```

The server speaks plain WebSocket/gRPC; see the TLS note in [section 5](#5-team-server-in-docker)
before exposing it beyond localhost.

## 2. Fedora, RHEL and derivatives (dnf)

```sh
sudo curl -fsSLo /etc/yum.repos.d/anchor.repo https://David-A-Phillips.github.io/anchor/anchor.repo
sudo dnf install anchor-context
```

The `.repo` file turns on `repo_gpgcheck` (the repository metadata is signed; the key is fetched
from `https://David-A-Phillips.github.io/anchor/rpm/RPM-GPG-KEY-anchor` and dnf asks you to confirm its
fingerprint). The individual RPMs are not signed: integrity comes from the signed metadata and
from the checksums and provenance attestation on the GitHub Release. Install one file directly:
`sudo dnf install ./anchor-context-<version>-1.x86_64.rpm`.

- **Upgrade:** `sudo dnf upgrade anchor-context`
- **Uninstall:** `sudo dnf remove anchor-context` (this also removes the `anchor` account and
  `/var/lib/anchor`; `/etc/anchor/anchor.toml` stays if you changed it, as `.rpmsave`).
- The same files and systemd units as the Debian package; see [Running as a
  service](#running-as-a-service) above. Man pages are marked as documentation: if your image
  sets `tsflags=nodocs` (containers often do) they are skipped.

## 3. Homebrew (macOS and Linux)

```sh
brew install David-A-Phillips/tap/anchor        # taps David-A-Phillips/homebrew-tap on first use
```

- **Upgrade:** `brew update && brew upgrade anchor`
- **Uninstall:** `brew uninstall anchor` (and `brew untap David-A-Phillips/tap` if you wish).
- **Service:** `anchor init` first, then `brew services start anchor` (launchd on macOS, systemd
  user service on Linux); logs in `$(brew --prefix)/var/log/anchor.log`.
- Completions (bash, zsh, fish) and the man page are installed by the formula. The formula installs
  the prebuilt release archive; `packaging/homebrew/anchor-source.rb` is a build-from-source
  variant.

## 4. Install script

```sh
curl -fsSL https://raw.githubusercontent.com/David-A-Phillips/anchor/main/scripts/install.sh \
  | ANCHOR_REPO=David-A-Phillips/anchor sh
```

The script detects your OS and CPU, downloads `anchor-<version>-<target>.tar.gz` and its
`.sha256` from the GitHub Release, verifies the checksum, and installs `anchor` into
`~/.local/bin`. No completions or man page; for those use a package.

| Variable | Meaning |
|----------|---------|
| `ANCHOR_REPO` | GitHub `owner/name` (default is the placeholder `David-A-Phillips/anchor`) |
| `ANCHOR_VERSION` | Version without the `v` (default: latest release) |
| `ANCHOR_INSTALL_DIR` | Install directory (default `~/.local/bin`) |
| `ANCHOR_BASE_URL` | Directory URL holding the archive and checksum, for a mirror or air-gapped host; requires `ANCHOR_VERSION` |

- **Upgrade:** run the script again. **Uninstall:** `rm ~/.local/bin/anchor`.
- Linux binaries are glibc builds (not static musl), built on Ubuntu 22.04: they need glibc 2.35
  or newer.
- Releases also carry `SHA256SUMS` (and `SHA256SUMS.asc`, a detached GPG signature, when the
  maintainers configured a signing key) and build-provenance attestations
  (`gh attestation verify <file> --repo David-A-Phillips/anchor`).

## 5. Team server in Docker

```sh
export ANCHOR_SYNC_TOKEN="$(openssl rand -hex 32)"
docker build -t anchor .
docker run -d --name anchor -p 7891:7891 \
  -e ANCHOR_SYNC_TOKEN -v anchor-data:/data anchor
```

The image runs `anchor server` as a non-root user. Data (database, device keys) lives in the
`/data` volume; the baked-in config is `/etc/anchor/anchor.toml` (`mode = "server"`, listens on
`0.0.0.0:7891`, bearer token from `ANCHOR_SYNC_TOKEN`). To use your own config, mount it and set
`ANCHOR_CONFIG`. `packaging/docker-compose.yml` shows the same with a named volume and an optional
Caddy service (`--profile tls`). **Upgrade:** rebuild and recreate the container; the volume keeps
the data. **Uninstall:** `docker rm -f anchor && docker volume rm anchor-data`.

**TLS.** The Anchor server does **not** terminate TLS: it accepts plain WebSocket and gRPC on
`sync.listen`. The client does speak TLS (`wss://` for the WebSocket transport, `https://` for
gRPC, verified against the public web PKI roots, so a self-signed certificate will not be
accepted by the client). Anything beyond a trusted LAN should therefore sit behind a TLS-terminating
reverse proxy with a publicly trusted certificate, for example Caddy:

```
anchor.example.com {
    reverse_proxy anchor:7891
}
```

(for `transport = "grpc"` use `reverse_proxy h2c://anchor:7891`). Clients then use
`server_url = "wss://anchor.example.com/sync"`. The bearer token is sent in the clear without TLS,
so never expose the plain port to the internet.

## 6. Cargo and from source

Requires Rust 1.88 or newer. No OpenSSL, cmake or protoc is needed (TLS is rustls, SQLite is
bundled, protobuf is compiled by `protox`); a C compiler is needed for SQLite, zstd and
tree-sitter.

```sh
cargo binstall --git https://github.com/David-A-Phillips/anchor anchor      # prebuilt archive (untested)
cargo install --git https://github.com/David-A-Phillips/anchor --locked      # compile from source
git clone https://github.com/David-A-Phillips/anchor && cd anchor
cargo install --path . --locked      # or: cargo build --release
```

`cargo install anchor` (from crates.io) only works once the crate is published, which needs
`publish = true`, a free crate name and a license (see `RELEASING.md`). **Upgrade:** rerun the
command (`--force` for `cargo install`). **Uninstall:** `cargo uninstall anchor`.

## 7. Windows

Not yet exercised on real Windows machines (the release workflow builds the archive).

```powershell
# Manual: download anchor-<version>-x86_64-pc-windows-msvc.zip from the GitHub Release,
# verify it against the .sha256 file, unpack it and put anchor.exe on your PATH.

# Scoop (manifest in packaging/scoop/anchor.json; add it to a bucket first):
scoop install https://raw.githubusercontent.com/David-A-Phillips/anchor/main/packaging/scoop/anchor.json

# winget (after the manifest in packaging/winget/ is accepted by winget-pkgs):
winget install David-A-Phillips.Anchor
```

Upgrade: `scoop update anchor` / `winget upgrade David-A-Phillips.Anchor`. Uninstall: `scoop uninstall anchor`
/ `winget uninstall David-A-Phillips.Anchor`. There is no Windows service wrapper yet; run `anchor daemon` from
a terminal or a Task Scheduler task.

Arch Linux: `packaging/aur/PKGBUILD` (package `anchor-context-bin`, untested). Nix:
`packaging/nix/flake.nix` (untested).

## 8. Connect an AI agent: `anchor mcp install`

```sh
anchor mcp install                       # Claude Code (runs `claude mcp add`)
anchor mcp install --client all          # Claude Code, Claude Desktop and Cursor
anchor mcp install --client cursor --print   # show what would change; change nothing
anchor --config ~/team/anchor.toml mcp install --scope user
anchor mcp uninstall --client all
```

- Registers the absolute path of the running `anchor` binary, plus `--config <path>` if you passed
  one, as an MCP server named `anchor` (`--name` changes it).
- Claude Code: uses the `claude` CLI if it is on `PATH`; otherwise prints the command to run.
  `--scope local|user|project` is passed through.
- Claude Desktop (`<config dir>/Claude/claude_desktop_config.json`) and Cursor
  (`~/.cursor/mcp.json`): the `mcpServers` entry is merged into the JSON file, every other key is
  preserved, a `.bak` copy is written first, and a file that is not valid JSON is refused untouched.
  Re-running is a no-op. Restart the application afterwards.

Plain `anchor mcp` still serves the protocol on stdio.

## 9. Share with a teammate in 5 minutes

One person (the host) runs the server; everyone else is a client.

**Host (once):**

1. Start the server as in [section 5](#5-team-server-in-docker) (Docker) or [section 1](#running-as-a-service) (systemd) on a machine your team can reach. Put TLS in front of it if it
   crosses the internet. Note the URL and the token.
2. On your own machine, configure the client and publish your sources.

**Teammate:**

1. Install: any channel above.
2. `anchor init`, then edit the config (`~/.config/anchor/anchor.toml`):

   ```toml
   mode = "client"

   [sync]
   server_url = "wss://anchor.example.com/sync"   # or ws://host:7891/sync on a trusted LAN
   auth_token_env = "ANCHOR_SYNC_TOKEN"
   subscribe = []        # empty: pull everything the server offers
   publish = []          # source ids this device uploads (e.g. ["payments-api"])
   ```

3. `export ANCHOR_SYNC_TOKEN=<the token the host gave you>` (add it to your shell profile).
4. `anchor sync` runs one sync and prints the result; `anchor daemon` keeps syncing in the
   background and serves the dashboard on <http://localhost:7890>.
5. `anchor query "Who owns the payments-api service?"`, and `anchor mcp install` to give your
   agent the same graph.

Locally configured trust levels are never overridden by what the server sends.

## 10. Air-gapped sharing with `.contextpack`

No server needed: export a signed snapshot and hand over the file.

```sh
anchor export -o team-graph.contextpack     # on the source machine
anchor import team-graph.contextpack        # on the receiving machine
```

Import verifies the signature and merges the pack (last-writer-wins per unit). The file can travel
by USB stick, shared drive or email.

## What has been tested

| Channel | Tested on | Result |
|---------|-----------|--------|
| `.deb` + apt repo (unsigned, `[trusted=yes]`) | `debian:bookworm-slim`, `ubuntu:24.04` containers, amd64 | install, `anchor --version/--help/demo`, man page, completions, units, conffile, remove, purge all pass |
| apt repo signed with a throwaway key, `Signed-By` keyring | same containers | pass |
| `lintian` on the `.deb` | Debian bookworm | no errors (one embedded-library override for a Rust YAML crate); one warning (initial-upload-closes-no-bugs) |
| `.rpm` + `createrepo_c` repo | `fedora:latest` container, x86_64 | install from file and from the repo, files, config file, account, remove all pass |
| Homebrew formula | Linuxbrew on Linux x86_64, local tap and local archive server | `brew audit --strict`, install, `brew test`, uninstall pass |
| Install script | Linux x86_64 (earlier work) | pass |
| Docker image | Linux x86_64 (earlier work) | pass |
| arm64 / aarch64 `.deb`, `.rpm`, brew | not built or run | untested |
| macOS (brew, archives), Windows (archives, Scoop, winget) | not run | untested |
| AUR `PKGBUILD`, Nix flake, `cargo binstall`, crates.io | not run | untested |
| Release workflow (all jobs), Pages publishing, tap update | YAML validated with `actionlint`, never executed | untested |
| PPA, Debian proper, homebrew-core, winget-pkgs submissions | not attempted | see `RELEASING.md` |
