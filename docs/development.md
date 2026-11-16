# Building and testing

## Requirements

- Rust 1.88 or newer and a C compiler. No OpenSSL headers, cmake or protoc are needed.
- Linux is the tested platform. Managed Ollama installation is Linux-only; elsewhere install
  Ollama yourself and set `ollama.base_url`.
- No language model is required: without one, Anchor classifies questions with heuristics and
  answers ownership and "why is it blocked" questions directly from the graph.

## Build

```sh
cargo build --release          # binary at target/release/anchor
cargo build --release --no-default-features   # without the Candle embedding backend
```

## Testing

```sh
cargo test --all-features                          # everything hermetic (no network)
cargo test --all-features --test performance -- --nocapture   # budgets, with measurements
cargo clippy --all-targets --all-features -- -D warnings
```

The performance suite asserts: file change → graph mutation p95 under 50 ms, idle CPU ~0 %,
resident memory under 3 GB during 2,000 concurrent queries, and projects the SQLite and
`.contextpack` footprint to 1,000 repositories.

Opt-in tests that need the network:

```sh
ANCHOR_E2E_OLLAMA=1 cargo test --all-features e2e_real_ollama -- --ignored --nocapture
```

downloads the real Ollama build (~1.5 GB) into `target/tmp`, pulls a tiny model, generates, tears
down, rebuilds and verifies again.

`scripts/fetch-corpus.sh` fetches a real-world corpus (Linux, Kubernetes, Grafana, exported pull-
request reviews) into `./corpus` for scale testing.

## Measured retrieval quality

`cargo test --all-features --test e2e -- --nocapture` scores 166 generated questions with known
answers by node key. Current results: ownership recall 1.00 and top-answer accuracy 0.99, with
stale reference-only claims never winning; multi-hop relationship questions recall 1.00 and
precision 1.00; justification recall 0.66 by key (the neutral, text-based score below is higher,
because the reason is usually returned on the answer rather than as a separate comment node).

## Known limitations

- Relation chains are read by rules: a multi-hop question worded outside the phrases in
  `src/engine/router/relation.rs` falls back to neighbourhood retrieval (or to the System 1
  model when it is enabled).
- A file or endpoint name shared by several repositories is answered as ambiguous unless the
  question names the service.
- Context precision is low (about 0.2 for ownership): the context includes neighbouring evidence
  beyond the answer.
- Two graph nodes that already exist are not merged retroactively when a later source reveals
  they are the same entity; `GraphStore::rebuild` re-materialises the graph and heals this.
- Wiki prose is linked to entities it mentions, but ownership stated only in prose is not turned
  into an ownership claim.
- Managed Ollama installation supports Linux only.
