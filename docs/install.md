# Installing and sharing Anchor

Platform status: only **Linux x86_64 (WSL2)** has been built and tested by hand. The release
workflow also builds Linux aarch64, macOS (arm64, x86_64) and Windows x86_64 archives, but those
have not been exercised yet. A managed Ollama install (`anchor setup`) is Linux-only; elsewhere
install Ollama yourself and point `ollama.base_url` at it.

## 1. Prebuilt binary (install script)

```sh
curl -fsSL https://raw.githubusercontent.com/OWNER/anchor/main/scripts/install.sh \
  | ANCHOR_REPO=OWNER/anchor sh
```

Replace `OWNER/anchor` with the real GitHub repository. The script detects your OS and CPU,
downloads `anchor-<version>-<target>.tar.gz` and its `.sha256` from the GitHub Release, verifies
the checksum, and installs `anchor` into `~/.local/bin`.

| Variable | Meaning |
|----------|---------|
| `ANCHOR_REPO` | GitHub `owner/name` (default is the placeholder `OWNER/anchor`) |
| `ANCHOR_VERSION` | Version without the `v` (default: latest release) |
| `ANCHOR_INSTALL_DIR` | Install directory (default `~/.local/bin`) |
| `ANCHOR_BASE_URL` | Directory URL holding the archive and checksum, for a mirror or air-gapped host; requires `ANCHOR_VERSION` |

Windows: download `anchor-<version>-x86_64-pc-windows-msvc.zip` from the release page, unpack it
and put `anchor.exe` on your `PATH`. Verify with the `.sha256` file.

Linux binaries are glibc builds (not static musl); they need a reasonably recent distribution
(built on Ubuntu 22.04).

## 2. Homebrew

`packaging/homebrew/anchor.rb` is a formula template for a tap (`OWNER/homebrew-tap`). Fill in the
version and the four SHA-256 values from the release's `.sha256` files, then
`brew install OWNER/tap/anchor`. It has not been run through `brew audit`.

## 3. From source

Requires Rust 1.88 or newer. No OpenSSL, cmake or protoc is needed (TLS is rustls, SQLite is
bundled, protobuf is compiled by `protox`); a C compiler is needed for SQLite, zstd and
tree-sitter.

```sh
git clone https://github.com/OWNER/anchor && cd anchor
cargo install --path . --locked      # or: cargo build --release
```

## 4. Team server in Docker

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
Caddy service (`--profile tls`).

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

## 5. Connect an AI agent: `anchor mcp install`

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

## 6. Share with a teammate in 5 minutes

One person (the host) runs the server; everyone else is a client.

**Host (once):**

1. Start the server as in section 4 on a machine your team can reach. Put TLS in front of it if it
   crosses the internet. Note the URL and the token.
2. On your own machine, configure the client and publish your sources.

**Teammate:**

1. Install: section 1.
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

## 7. Air-gapped sharing with `.contextpack`

No server needed: export a signed snapshot and hand over the file.

```sh
anchor export -o team-graph.contextpack     # on the source machine
anchor import team-graph.contextpack        # on the receiving machine
```

Import verifies the signature and merges the pack (last-writer-wins per unit). The file can travel
by USB stick, shared drive or email.
