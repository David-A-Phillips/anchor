# Hosting Anchor for a team

One machine runs `anchor server`; everyone else runs Anchor in client mode and syncs with it.
The server holds the shared graph. Clients keep working offline and can hold each source at a
different depth, pulling more from the server when a question needs it.

## What you need

- A machine reachable by the team, with a persistent data directory.
- A shared bearer token (any long random string).
- A reverse proxy for TLS. The sync server speaks plain WebSocket and gRPC; it does not terminate
  TLS itself, and clients only trust public certificate authorities, so put a proxy with a real
  certificate in front of it for anything beyond a private network.

## Run the server

With Docker:

```sh
docker build -t anchor .
docker run -d --name anchor-server \
  -p 7891:7891 -v anchor-data:/data \
  -e ANCHOR_SYNC_TOKEN="$(openssl rand -hex 32)" \
  anchor
```

`packaging/docker-compose.yml` runs the same container behind Caddy, which obtains a certificate
and forwards `wss://` traffic (`packaging/Caddyfile`). The compose file has not been run end to
end; the plain `docker run` path has (sync over WebSocket, token rejection, non-root user).

Without Docker, on the server:

```toml
# /etc/anchor/anchor.toml
mode = "server"
data_dir = "/var/lib/anchor"

[sync]
listen = "0.0.0.0:7891"
auth_token_env = "ANCHOR_SYNC_TOKEN"
```

```sh
ANCHOR_SYNC_TOKEN=... anchor --config /etc/anchor/anchor.toml server
```

The Debian and RPM packages ship a hardened systemd unit for this; see
[install.md](install.md).

## Connect a client

```toml
mode = "client"

[sync]
server_url = "wss://anchor.example.com/sync"   # ws://host:7891/sync on a private network
auth_token_env = "ANCHOR_SYNC_TOKEN"
publish = ["payments-api"]        # local sources this device contributes
subscribe = []                    # empty: everything the server has
default_depth = "phone_book"      # how much of sources that are not on this machine to materialise
interval_secs = 300               # the daemon syncs this often
```

```sh
export ANCHOR_SYNC_TOKEN=...
anchor sync          # once; `anchor daemon` keeps syncing
```

Devices exchange signed deltas. Merging is last-writer-wins per unit, and trust levels configured
locally are never overridden by a peer. Each device has an Ed25519 identity under
`data_dir/keys`. Listing peers' public keys in `trusted_devices.json` there restricts sync to
known devices (there is no command for editing that file yet); with no list, any valid signature
is accepted and the token is the gate.

## Who ingests what

The usual split: a CI job or the server itself ingests the shared sources (the organisation
model, the wiki and tracker exports, the main repositories) and publishes them; developers
publish nothing or only their own repositories, and subscribe at `phone_book` depth so the whole
organisation costs a few units per repository. A question that touches a shallow source pulls the
deeper tile from the server (see "Depth with a sync server" in
[data-sources.md](data-sources.md)).

To keep third-party sources fresh on the server, run the daemon there with `[[github]]` and
`[[atlassian]]` entries; it refreshes them on their `interval_minutes` and rescans.

## Sharing without a server

```sh
anchor export -o team.contextpack     # signed, compressed snapshot of the graph
anchor import team.contextpack        # on another machine; verifies the signature, then merges
```

## Limits

- No per-user access control: everyone with the token can read everything the server holds.
  Confluence pages with read restrictions are skipped at export for that reason unless you opt
  in.
- TLS has been verified only as far as the client opening a TLS connection for `wss://` and
  `https://` URLs; a full session through a proxy has not been tested.
- One server process; no clustering.
