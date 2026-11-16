# Models and third-party APIs

Anchor works without any language model: questions are classified by rules and answered from the
graph. Models are optional, and none of them sits on the path of an ordinary question.

There are two tiers. **System 1** is a small, fast model (local by default) used for
classification fallback and for tagging at ingestion. **System 2** is a larger model used only
when you ask for a written explanation. Either tier can be a local Ollama model, Anthropic's API,
or any OpenAI-compatible endpoint.

## Choosing providers

No models:

```toml
[models.system1]
provider = "disabled"
[models.system2]
provider = "disabled"
```

Everything local with Ollama:

```toml
[models.system1]
provider = "ollama"
model = "llama3.2:3b"
base_url = "http://127.0.0.1:11434"

[models.system2]
provider = "ollama"
model = "llama3.1:8b"
```

Anthropic for the heavy tier:

```toml
[models.system2]
provider = "anthropic"
model = "claude-opus-5-5"
api_key_env = "ANTHROPIC_API_KEY"     # the NAME of the variable; the key is never written to disk
```

Any OpenAI-compatible API (OpenAI, Azure OpenAI, OpenRouter, vLLM, LM Studio, llama.cpp server):

```toml
[models.system2]
provider = "openai_compatible"
model = "gpt-4.1"
base_url = "https://api.openai.com/v1"   # or your gateway
api_key_env = "OPENAI_API_KEY"
```

Each tier also takes `timeout_secs`, `max_tokens` and `temperature`.

Only the Ollama provider has been exercised against a real server. The Anthropic and
OpenAI-compatible providers are covered by tests against mocked responses; no real
cloud call has been made from this codebase yet.

## Keeping cloud use under control

```toml
[models.routing]
allow_cloud = true              # false: never leave the machine, whatever the tiers say
escalation_confidence = 0.6     # confidence threshold used when routing between the tiers
max_cloud_usd_per_day = 5.0     # hard daily cap; further cloud calls are refused
```

`anchor query --local-only` forbids the cloud for one question. `anchor spend` shows tokens and
estimated cost per tier and model; the dashboard charts the same. What is sent to a cloud model
is the question and the retrieved evidence for it, never the whole graph.

## What the models do

Models stay off the interactive path. Questions are classified by rules in microseconds and
answered straight from the graph; nothing waits on a model unless you ask for prose.

| Where | What the model does | When |
|-------|---------------------|------|
| Classification | The local System 1 model reads a question only when the rules cannot classify it (multi-hop "who leads the team that owns…" questions). Its reading is cached, so a repeated question is instant. | At question time, as a fallback |
| Answers | `anchor query --explain` (or `query.model_answers = true`) has a model write a prose answer from the retrieved evidence. | Only on request |
| Enrichment | `anchor enrich` has System 1 summarise and tag wiki pages, tickets, pull requests and APIs, and name the entities they are about. Tags become searchable aliases; named entities become links. Stored as a `reference_only` source, so it never overrides an authoritative fact. | In the background, after ingestion (the daemon does it automatically) |

Models are never used to search the graph.

To measure what they change, generate a question set with known answers and score it with and
without a model configured:

```sh
anchor demo --out /tmp/demo --scale small
anchor --config /tmp/demo/anchor.toml ingest
anchor --config /tmp/demo/anchor.toml eval --truth /tmp/demo/ground_truth.json
```

`anchor eval` reports, per question category, how often the intent was read correctly, context
recall, whether the top answer was right, latency per question, and model calls and tokens.

## Managing Ollama

`anchor ollama provision` uses the server at `ollama.base_url` if one answers. Otherwise it
installs the official build into `data_dir/ollama` (user space, no sudo), starts it, pulls
`ollama.default_model` and verifies a generation. An external server is never stopped or wiped.

`anchor ollama rebuild` is the bootstrap resilience test: it wipes the managed installation and
models, reinstalls, re-pulls and verifies, and prints per-phase timings. `--keep-binary` and
`--keep-downloads` narrow what is wiped. `anchor ollama teardown` only removes.
