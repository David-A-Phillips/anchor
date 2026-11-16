# syntax=docker/dockerfile:1
# Anchor team sync server in one container.
#
#   docker build -t anchor .
#   docker run -p 7891:7891 -e ANCHOR_SYNC_TOKEN=change-me -v anchor-data:/data anchor
#
# The server speaks plain WebSocket/gRPC on 7891 and does not terminate TLS itself: put a
# TLS-terminating reverse proxy in front for anything beyond a trusted network
# (see packaging/docker-compose.yml).

FROM rust:1-bookworm AS builder
WORKDIR /src
# Cache dependency compilation: build a stub first, then the real sources.
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
RUN mkdir -p src benches && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs \
    && echo 'fn main() {}' > benches/engine.rs \
    && cargo build --release --locked || true
COPY src ./src
COPY benches ./benches
RUN touch src/main.rs src/lib.rs && cargo build --release --locked --bin anchor

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin anchor \
    && mkdir -p /data /etc/anchor && chown anchor:anchor /data
COPY --from=builder /src/target/release/anchor /usr/local/bin/anchor
COPY packaging/docker/anchor.toml /etc/anchor/anchor.toml

ENV ANCHOR_HOME=/data \
    ANCHOR_CONFIG=/etc/anchor/anchor.toml
USER anchor
VOLUME ["/data"]
EXPOSE 7891
CMD ["anchor", "server"]
