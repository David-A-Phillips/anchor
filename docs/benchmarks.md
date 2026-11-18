# Benchmarks

How Anchor compares with other retrieval systems, how the numbers were produced, and what they
do and do not show. There are two kinds of benchmark here:

- **Anchor's own corpus** (most of this page): a generated organisation with code, an org chart,
  wikis, tickets and reviews, and 735 questions with known answers. It measures what Anchor is
  for, and it was built by the same project, so it favours Anchor.
- **[Standard multi-hop benchmarks](#standard-multi-hop-benchmarks)**: MuSiQue, 2WikiMultiHopQA
  and HotpotQA, the passage-retrieval sets graph-RAG papers publish scores on. Neither the data
  nor the questions were chosen by this project; three ranking constants were chosen on the
  first 300 questions of each, and the remaining 700 are reported separately.

Everything here can be reproduced with `anchor compare` and the scripts in
[`scripts/compare/`](../scripts/compare). Read the [caveats](#caveats) before quoting a number.

## Summary

100-repository synthetic enterprise, 735 questions. "Reader answers correctly" is the share of
expected answers present in an answer written by the same local model (`llama3.1:8b`) from each
system's retrieved context, on a 131-question sample.

| System | Answer in retrieved context | Reader answers correctly | Tokens per question | Query time |
|---|---|---|---|---|
| **Anchor** | **0.99** | **0.85**\* | 1,287 | 4 ms |
| LlamaIndex hybrid (vector + BM25) | 0.89 | 0.49 | 3,100 | 30–50 ms |
| LlamaIndex BM25 | 0.85 | 0.44 | 3,646 | 0.6 ms |
| LlamaIndex vector | 0.64 | 0.41 | 2,125 | 30–75 ms |
| LangChain hybrid | 0.70 | 0.42 | 1,950 | 20–40 ms |
| LangChain BM25 | 0.63 | 0.35 | 2,170 | 2 ms |
| LangChain vector | 0.66 | 0.34 | 1,563 | 14–20 ms |
| LightRAG (graph RAG, `mix` mode) | 0.69 | 0.32 | 5,305 | 430 ms |
| Plain full-text over Anchor's chunks | 0.49 | 0.22 | 1,863 | 5 ms |

| System | Index on disk | Peak memory | Indexing time |
|---|---|---|---|
| **Anchor** | 12.7 MB | 164 MB | 1.1 s |
| LangChain (FAISS + BM25) | 8.7 MB | 200 MB | 24–43 s |
| LlamaIndex (vector + BM25) | 16.5 MB | 323 MB | 18–27 s |
| LightRAG | 54.3 MB | 1,067 MB | about 1 h 40 min |

\* Retrieval figures, footprint and query times were rerun for 0.1.2. The reader-model figures
for Anchor are from the 0.1.1 run: 0.1.2 labels an ownership question's secondary candidates (the
team owning the enclosing component, the domain lead) as evidence instead of answers, which
changed Anchor's context text for 40 of the 131 sample questions, and the shared reader has not
been rerun on them yet. The other systems' contexts did not change.

The LangChain, LlamaIndex and LightRAG rows leave out the models they call, which run in a
separate Ollama server: the embedding model (`nomic-embed-text`, 274 MB on disk) for all three,
plus the 4.9 GB language model for LightRAG. Anchor's default embeddings are computed in-process
and need no model.

## Results by question type

Reader answers correctly (sample sizes in the heading; Anchor's "direct" column is the share it
answers from the graph with no model at all, over all questions of that type):

| Question type | Questions (sample) | Anchor | Anchor direct | LlamaIndex hybrid | LangChain hybrid | LightRAG |
|---|---|---|---|---|---|---|
| Ownership ("who owns X?") | 279 (41) | 0.88 | 0.97 | 0.32 | 0.37 | 0.15 |
| Justification ("why was X blocked / dismissed?") | 95 (30) | 0.80 | 1.00 | 0.60 | 0.50 | 0.33 |
| Relationships (multi-hop) | 283 (30) | 1.00 | 1.00 | 0.60 | 0.30 | 0.20 |
| Prose ("how do I ...", half paraphrased) | 78 (30) | 0.70 | none | 0.50 | 0.53 | 0.67 |

Answer in retrieved context, all questions of each type:

| Question type | Anchor | LlamaIndex hybrid | LlamaIndex BM25 | LangChain hybrid | LightRAG |
|---|---|---|---|---|---|
| Ownership | 0.97 | 0.75 | 0.63 | 0.86 | 0.68 |
| Justification | 1.00 | 1.00 | 0.98 | 0.77 | 0.64 |
| Relationships | 1.00 | 0.97 | 0.99 | 0.44 | 0.63 |
| Prose | 0.99 | 1.00 | 1.00 | 1.00 | 1.00 |

**Conflicting sources.** Twelve ownership questions have a stale wiki page naming the wrong
owner. With Anchor's context the reader led with the correct owner 12 of 12 times; with any
other system's context, at most 1 of 12. Plain retrieval has no way to know which source is
authoritative; this is what trust levels are for.

## The metrics other projects define

None of LlamaIndex, LangChain, LightRAG, GraphRAG or Ragas publishes speed or footprint figures
in its repository or documentation (checked November 2026), so there are no upstream numbers to
match on those. LightRAG is the only one that publishes benchmark results at all, as LLM-judged
win rates. The quality metrics they define, computed on this run:

| System | Hit rate | MRR | Precision | nDCG | Context precision |
|---|---|---|---|---|---|
| Anchor | 0.99 | 0.93 | 0.16 | 0.91 | 0.84 |
| LlamaIndex hybrid | 0.89 | 0.45 | 0.12 | 0.54 | 0.40 |
| LlamaIndex BM25 | 0.85 | 0.57 | 0.12 | 0.61 | 0.50 |
| LlamaIndex vector | 0.64 | 0.34 | 0.08 | 0.40 | 0.31 |
| LangChain hybrid | 0.70 | 0.37 | 0.09 | 0.44 | 0.34 |
| LangChain BM25 | 0.63 | 0.32 | 0.08 | 0.38 | 0.30 |
| LangChain vector | 0.67 | 0.36 | 0.09 | 0.42 | 0.33 |

- Hit rate, MRR, precision and nDCG are the retrieval metrics of LlamaIndex's
  `RetrieverEvaluator`; context precision is Ragas' non-LLM variant (mean precision@k over the
  relevant ranks).
- Those libraries judge relevance by chunk id. Every system here chunks differently, so a context
  counts as relevant when it contains an expected answer.
- LightRAG returns one block of text per question, so rank metrics do not apply to it.
- Anchor's MRR is helped by its direct answer being the first thing it returns. Its precision is
  low because most of what it returns is supporting evidence, not the answer.

**LightRAG's own method** is a pairwise LLM judge that picks the better of two answers for
comprehensiveness, diversity and empowerment. `scripts/compare/judge.py` reproduces its prompt.
With `llama3.1:8b` as judge on 101 questions, Anchor's answers won overall 49% of the time
against LlamaIndex hybrid, 47% against LlamaIndex BM25 and 53% against LangChain hybrid: a tie.
Those criteria reward long, many-sided answers and do not check correctness, so they say little
about factual questions. (This judging run predates the prose questions and was not repeated
against LightRAG.)

Not computed: Ragas' LLM-judged metrics (faithfulness, answer relevancy, LLM context recall).

## Method

**Neutral format.** Every system reads the same corpus directory and writes the same result
file, so nothing in the scoring depends on Anchor's internals:

- `questions.jsonl`: the question, plus the names and aliases of each expected answer and of
  each known-stale answer. For justification and prose questions the expected answer is a phrase
  from the source text.
- `results/<system>.jsonl`: per question, the text contexts the system retrieved, best first,
  optionally its own answer, and the shared reader's answer.
- `results/<system>.meta.json`: index size on disk, peak resident memory of the process that
  indexed and answered, and indexing time.

**Scoring** (`src/cli/compare.rs`, unit-tested):

- Each system's contexts are cut to the same budget: at most 50 contexts and 6,000 tokens
  (four characters per token). Systems choose how many contexts to return and how large.
- Matching is on normalised text (lower case, punctuation folded to spaces, whole words). An
  expected answer is found when any of its forms appears. Phrases must appear verbatim in
  contexts; in generated answers 70% of a phrase's longer words suffice, since readers
  paraphrase.
- "Answer in retrieved context" is the mean share of expected answers found in the budgeted
  context. It does not check that the context connects the answer to the question.
- "Reader answers correctly" is the same share, measured on the answer the shared reader wrote.
- For conflict questions, a result is safe when the stale owner is absent or appears after the
  correct one.

**Shared reader** (`scripts/compare/generate.py`): `llama3.1:8b` through Ollama, temperature 0,
8,192-token window, one fixed prompt for every system: answer from the context only, be brief,
prefer the claim the context marks as more authoritative, say "unknown" otherwise. The sample is
every conflict question plus an evenly spaced 30 per category, the same for every system.

**Systems as configured**

| System | Version | Configuration |
|---|---|---|
| Anchor | this repository | Default config, no models, hash embeddings. Returns its direct answer (when it has one), resolved conflicts, then evidence. |
| Plain baselines | this repository | BM25, vector and reciprocal-rank fusion over Anchor's own chunks, no graph and no trust. |
| LlamaIndex | 0.14.26 | `SimpleDirectoryReader`, default sentence splitter, `VectorStoreIndex`, `BM25Retriever`, `QueryFusionRetriever` (reciprocal rerank), top 12. |
| LangChain | 1.5.0 | `DirectoryLoader` + `TextLoader`, `RecursiveCharacterTextSplitter` (1,000 / 200), FAISS, `BM25Retriever`, reciprocal-rank fusion of both, top 12. |
| LightRAG | 1.5.7 | `mix` mode, context only, 6,000-token cap, no reranker; `llama3.1:8b` for extraction and query keywords. |

Embeddings for the three frameworks: `nomic-embed-text` through Ollama.

**Hardware.** One workstation: 16 cores, 15 GB RAM, RTX 3090 Ti, Linux under WSL2. The GPU was
shared between jobs, which is why the vector and hybrid query times are given as ranges; their
time is dominated by the call to the embedding model.

## Dataset and questions

The corpus is generated by `anchor demo` (`src/synth/`), deterministic for a seed. The benchmark
uses `--scale medium --seed 7`:

- 100 repositories with 400 source files in Rust, Python, TypeScript, Go, Java and C, each with
  `CODEOWNERS` and a README;
- an organisation model (3 portfolios, 9 domains, 37 components, 100 services, 19 teams, 66 people) and a
  deliberately outdated legacy directory;
- 58 wiki pages (ownership pages, some stale; runbooks; design notes) with comment threads,
  including dismissed ones;
- an API portal with definitions and approval records, 112 Jira issues with blocking links, and
  56 pull requests with reviews, some dismissed.

About 1.5 MB of text in 878 files. Questions come from the generator's ground truth, so every
expected answer is known exactly:

| Type | Count | Examples |
|---|---|---|
| Ownership | 279 | "Who owns the X service?", "Who maintains src/y.rs in X?"; direct, inherited from a component, reference-only, and 12 with a stale conflicting claim |
| Justification | 95 | "Why is POST /v1/z in the X API blocked?", "Why was the review on X#114 dismissed?", "Why was the suggestion on '...' dismissed?" |
| Relationships | 283 | "Which portfolio does X belong to?", "Who leads the team that owns the component containing X?", "Which services consume the X API?", "Which domain owns the code that implements GET /v1/z?" |
| Prose | 78 | "What does the X runbook say to do when the queue depth stays above the alert threshold?" and paraphrases that avoid the page's wording |

## Reproducing

```sh
anchor demo --out corpus/compare/data --scale medium --seed 7 --no-history
mv corpus/compare/data/anchor.toml corpus/compare/data/ground_truth.json corpus/compare/
# edit data_dir in corpus/compare/anchor.toml, then:
anchor --config corpus/compare/anchor.toml compare corpus/compare/run \
    --truth corpus/compare/ground_truth.json --reindex     # Anchor and the plain baselines

python -m venv venv
venv/bin/pip install llama-index-core llama-index-embeddings-ollama llama-index-retrievers-bm25 \
    langchain langchain-community langchain-ollama langchain-text-splitters faiss-cpu rank_bm25 \
    lightrag-hku
ollama pull nomic-embed-text && ollama pull llama3.1:8b

cd scripts/compare
R=../../corpus/compare
python run_llamaindex.py $R/data $R/run --index-dir /tmp/index-llamaindex
python run_langchain.py  $R/data $R/run --index-dir /tmp/index-langchain
python run_lightrag.py   $R/data $R/run --workdir   /tmp/index-lightrag     # hours; resumable
python generate.py $R/run                      # shared reader writes an answer per system
python judge.py $R/run --against llamaindex-hybrid langchain-hybrid          # optional
anchor compare $R/run --kinds                  # scores, ranking metrics, footprint
```

To add another system, write `results/<name>.jsonl` in the format above;
`scripts/compare/common.py` has the helpers. To use your own questions, write `questions.jsonl`
by hand and skip `--truth`.

## What changed Anchor's numbers

Anchor was improved while this benchmark was being built, which matters for how much to trust
it:

| Stage | Answer in context | Answered directly |
|---|---|---|
| First comparison (657 questions) | 0.81 | 0.50 |
| After relation chains, scoped names and dismissal reasons | 0.99 | 0.99 |
| Prose questions added (78), before changes | 0.85 on prose | none |
| After quoting documentation pages for the named entity | 0.99 on prose | none |
| 0.1.2: secondary ownership candidates labelled as evidence | 0.99 (unchanged) | unchanged |

A second corpus generated with a different seed and never inspected during that work scores the
same (0.99 / 0.98 on the first three question types).

The 0.1.2 relabelling raised the share of answer-labelled items that are correct owners in the
key-based test (`tests/e2e.rs`) from 0.23 to 0.91, with recall unchanged at 1.00. It does not
move the text-based scores above, which look for the answer anywhere in the context.

**System 1 enrichment** (summaries, tags and "questions this text answers", written at ingestion
by `llama3.2:3b`) made no measurable difference: enriching all 276 pages, tickets, pull requests
and APIs took 3 min 43 s and 0.9 MB, and every score stayed within 0.01. The questions all name
the entity they are about, so the graph already finds the text.

## Other measurements

From `cargo test --test performance -- --nocapture` and manual runs on the same machine:

| Measurement | Result |
|---|---|
| File change to graph update, p95 | 12.5 ms (budget 50 ms) |
| Idle CPU with the watcher running | 0.001% of one core |
| Peak memory under 2,000 concurrent queries | 72 MB (budget 3 GB) |
| Kubernetes, full depth | 4 min 12 s, 393,710 nodes, 1.48 GB database |
| Kubernetes, map depth | 39 s, 266 MB |
| Kubernetes, phone-book depth | 1 s, 0.6 MB |
| Grafana with 1,494 commits of history and 124 exported pull requests | 5 min 8 s, 219,782 nodes, 1.09 GB |
| `anchor why` on a Grafana file (blame, commits, pull request, review threads) | about 2 s |

## Caveats

- **The corpus and questions were generated by this project.** They exercise what Anchor was
  built for: ownership, trust conflicts, typed multi-hop relationships, decisions with recorded
  reasons. Open-ended questions over long prose, where vector search is strongest, are barely
  represented, and the prose pages that exist are formulaic.
- **Anchor was tuned after seeing the first results.** The held-out corpus rules out fitting to
  one dataset, but it shares the question templates. Relation-chain questions are read by rules;
  a question worded outside the phrases those rules know falls back to weaker retrieval.
- **Every question names the entity it is about.** Questions that name nothing ("which service
  throttles by tenant?") are not tested, and that is where the lack of neural embeddings would
  show.
- **The other systems ran out of the box.** Default chunking, no reranker, a small embedding
  model. Tuning would raise their scores. LightRAG in particular ran on a model far smaller than
  its documentation recommends, on a shared GPU, with a few chunks timing out and an interrupted
  indexing run that was resumed; treat its row as a floor and its indexing time as an estimate.
- **"Answer in retrieved context" is a weak measure.** It only checks that the answer's name
  appears in up to 6,000 tokens of text. The reader-model column is the better signal, and it
  depends on one small local model with one prompt, on a sample of 131 questions; the conflict
  result rests on 12.
- **The reader prompt tells the model to prefer what the context marks as authoritative.** Only
  Anchor marks anything, so this instruction helps Anchor on conflicts; it is also how an agent
  would be instructed in practice.
- **Footprint is measured per process** and excludes model servers. Indexing times for the
  frameworks are almost entirely embedding calls and vary with GPU load.
- **One machine, one run each.** No confidence intervals.

The [standard multi-hop benchmarks](#standard-multi-hop-benchmarks) below are the independent
check. The fair next test beyond that is a set of questions written by people who did not build
Anchor, against real repositories, wikis and trackers. The harness accepts hand-written
questions and any system that writes a results file.

## Change impact

`anchor impact` and `anchor hotspots`, scored against the same corpus's ground truth
(`--scale medium --seed 7`, no models). Reproduce with
`ANCHOR_IMPACT_SCALE=medium ANCHOR_IMPACT_SEED=7 cargo test --release --test impact scores -- --nocapture`.

| Check | Result |
|---|---|
| A change to an API reaches every service that consumes it (recall) | 1.00 (125/125) |
| Services reported as consuming it directly are consumers (precision) | 1.00 (125/125) |
| The owner reported for a changed API or service is its owner | 1.00 (129/129) |
| Services flagged "single-person owner" are exactly those a person owns (precision / recall) | 1.00 / 1.00 (16) |
| Time per impact query, p95 | 0.3 ms |

The ground truth comes from the generator that also builds the graph, so this checks that impact
follows the relationships correctly; it says nothing about how complete those relationships are
in a real organisation's data.

## Standard multi-hop benchmarks

The passage-retrieval benchmark graph-RAG papers report: for each question, the share of its
gold supporting passages among the first 2 and first 5 retrieved (recall@2, recall@5), averaged
over 1,000 questions per dataset. Data: the release used by HippoRAG 2
([`osunlp/HippoRAG_2`](https://huggingface.co/datasets/osunlp/HippoRAG_2)): 1,000 questions each
from MuSiQue, 2WikiMultiHopQA and HotpotQA, and corpora of 11,656, 6,119 and 9,811 Wikipedia
passages (the gold and distractor passages of all the questions).

| System | MuSiQue | 2Wiki | HotpotQA | Average |
|---|---|---|---|---|
| **Anchor** (no model, default configuration) | 35.3 / 47.8 | 70.0 / 91.0 | 59.9 / 77.6 | 55.1 / 72.1 |
| Anchor 0.1.1 | 26.9 / 42.6 | 60.5 / 66.7 | 44.6 / 68.8 | 44.0 / 59.4 |
| BM25, this harness | 34.1 / 44.6 | 55.5 / 65.8 | 56.1 / 72.8 | 48.6 / 61.1 |
| *Published (HippoRAG 2 paper, Table 9):* | | | | |
| BM25 | 32.4 / 43.5 | 55.3 / 65.3 | 57.3 / 74.8 | 48.3 / 61.2 |
| Contriever | 34.8 / 46.6 | 46.6 / 57.5 | 58.4 / 75.3 | 46.6 / 59.8 |
| GTR (T5-base) | 37.4 / 49.1 | 60.2 / 67.9 | 59.3 / 73.9 | 52.3 / 63.6 |
| NV-Embed-v2 (7B) | 52.7 / 69.7 | 67.1 / 76.5 | 84.1 / 94.5 | 68.0 / 80.2 |
| RAPTOR (Llama-3.3-70B) | 47.0 / 57.8 | 58.3 / 66.2 | 76.8 / 86.9 | 60.7 / 70.3 |
| HippoRAG (Llama-3.3-70B) | 41.2 / 53.2 | 71.9 / 90.4 | 60.4 / 77.3 | 57.8 / 73.6 |
| HippoRAG 2 (Llama-3.3-70B) | 56.1 / 74.7 | 76.2 / 90.4 | 83.5 / 96.3 | 71.9 / 87.1 |

Recall@2 / recall@5, in percent. Published rows are from Gutiérrez et al., *From RAG to Memory:
Non-Parametric Continual Learning for Large Language Models* (ICML 2025, arXiv 2502.14802v2),
Table 9; the averages here are over these three datasets only. GraphRAG and LightRAG are not in
that table because they do not return ranked passages. Recall@10 for Anchor: 56.6, 95.0 and 88.6
(BM25: 52.2, 70.5, 86.2).

**What it shows.** Without any model, Anchor's average recall@5 (72.1) is above BM25's (61.1),
the small dense retrievers' (59.8, 63.6) and RAPTOR's (70.3), and below HippoRAG's (73.6) and
HippoRAG 2's (87.1), both of which build their graph with a 70B model. On 2WikiMultiHopQA, whose
questions name the entities they are about, it matches HippoRAG 2 (91.0 against 90.4). On
HotpotQA it is above BM25 and the small dense retrievers and well below a 7B embedding model; on
MuSiQue it is above BM25 and Contriever and below GTR and the larger systems. Its recall@2 is
above BM25's on all three.

**Where the gain comes from.** Wikipedia passages carry no owners, imports or explicit links, so
until this release Anchor's graph over them had nodes and no edges, and retrieval was its
full-text search. Two changes, each measured on its own:

| Change | MuSiQue | 2Wiki | HotpotQA |
|---|---|---|---|
| 0.1.1 | 26.9 / 42.6 | 60.5 / 66.7 | 44.6 / 68.8 |
| Ranking: fused search, relevance spread along links | 35.8 / 45.5 | 63.2 / 67.5 | 59.0 / 72.0 |
| ... and pages linked by the titles they name | 32.8 / 46.6 | 70.0 / 91.0 | 59.8 / 77.5 |
| ... with links through common titles weighted down | 35.3 / 47.8 | 70.0 / 91.0 | 59.9 / 77.6 |

- **Title links** (`graph.title_links`, on by default). After a full scan, every wiki page title
  is matched, case-sensitively and on word boundaries, against the text of every other page, and
  each match becomes a `Mentions` link. This needs no model and takes 0.5 to 1.8 seconds per
  corpus; it finds 2,120 links in 2WikiMultiHopQA, 4,867 in HotpotQA and 13,270 in MuSiQue. A
  title that more than ten pages name links with proportionally less weight, as inverse
  document frequency weights common words, so relevance flows mostly through specific links.
- **Ranking for open questions.** When a question names nothing Anchor can resolve and asks for
  no particular kind of answer, the node-name, full-text and (with a neural embedding model)
  vector searches are merged by reciprocal rank fusion, and each of the five best candidates
  passes three quarters of its score along its links: a one-step, personalised PageRank. A node
  name counts only when the question contains it whole: names that merely share a common word
  with the question cost HotpotQA six points of recall@2. Without links this mainly improves the
  top of the list; with links, the second passage of a two-hop question comes into the top five.
  Anchor's full-text search on its own scores as BM25 does (34.2 / 44.0, 56.1 / 66.0,
  56.5 / 73.4), so the gains are the ranking's and the links'.

Questions about owners, dependencies and the other answer kinds keep their previous seeding; on
the synthetic benchmark above every context and every answer is identical before and after.

**Models.** A local model can also find the links: with `llama3.1:8b` naming the entities each
passage refers to (`anchor enrich`), 2WikiMultiHopQA recall@5 rose from 66.7 to 78.2 under the
0.1.1 ranking. Keeping the model's summaries but deleting its links gave 66.0, so the links did
all of it; on that dataset exact titles find as many. Runs with model links, a neural embedding
model and both on all three datasets, under the new ranking, are prepared but not yet run.

**Method.** `scripts/bench/multihop.py` writes each passage to a Markdown file (`# title`, then
the text), ingests them as one wiki source with the default configuration and no model, and asks
every question with `anchor query --retrieval-only`. A passage's rank is the position of its page
among the returned items. The three ranking constants (`RRF_K` 10, `SPREAD` 0.75, `SPREAD_FROM`
5 in `src/engine/query/fusion.rs`) were chosen from eight settings, and the whole-name rule
tried, on the first 300 questions of each dataset; on the other 700 alone, Anchor scores
35.6 / 48.3, 70.0 / 90.9 and 59.9 / 76.8 (BM25: 35.0 / 45.7, 55.1 / 65.0, 56.2 / 72.6), in line
with the full sets. Tried on the same questions and not kept, because they were mixed or worse:
a higher weight for titles in full-text search, case-insensitive and base-name ("Paris" for
"Paris (film)") title matching, dropping links through common titles instead of weighting them,
and a second step of relevance spreading. A weekly CI job (`.github/workflows/multihop.yml`) reruns the benchmark and
fails below set scores, and `tests/multihop.rs` checks two-hop retrieval on a generated corpus
in every `cargo test`. The same script scores
an Okapi BM25 (k1 1.5, b 0.75) over the same passages, which reproduces the published BM25 figures
to within two points: a check that the corpus, gold passages and scoring match the papers'. A
question takes about 18 ms including process start; 3,000 take under half a minute with eight
parallel processes.

**Caveats.** Recall measures retrieval only, not answers. The published rows were run by the
paper's authors on their hardware with their implementation; only the two BM25 rows were run here.
Title links and whole-name matching help most where pages are named by their exact titles, as in
Wikipedia and in many engineering wikis; MuSiQue's questions and passages often use other names,
and gain least.

Reproduce:

```sh
mkdir -p data/hippo && cd data/hippo
for f in musique 2wikimultihopqa hotpotqa; do
  curl -LO https://huggingface.co/datasets/osunlp/HippoRAG_2/resolve/main/$f.json
  curl -LO https://huggingface.co/datasets/osunlp/HippoRAG_2/resolve/main/${f}_corpus.json
done
cd -
python scripts/bench/multihop.py prepare data/hippo corpus/multihop
for f in musique 2wikimultihopqa hotpotqa; do
  anchor --config corpus/multihop/$f/anchor.toml ingest
done
python scripts/bench/multihop.py run data/hippo corpus/multihop --anchor target/release/anchor
python scripts/bench/multihop.py run data/hippo corpus/multihop --anchor target/release/anchor \
    --offset 300    # the questions not used to choose the ranking constants
```
