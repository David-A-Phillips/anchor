#!/usr/bin/env python3
"""Passage retrieval on the standard multi-hop QA benchmarks (MuSiQue, 2WikiMultiHopQA,
HotpotQA), scored the way graph-RAG papers report it: recall@2 and recall@5 of the gold
supporting passages, averaged over 1,000 questions per dataset (recall@10 is reported too).

The data is the release used by HippoRAG 2 (https://huggingface.co/datasets/osunlp/HippoRAG_2):
`<name>.json` (questions with their gold passages) and `<name>_corpus.json` (every passage the
retriever searches: the gold and distractor passages of all the questions).

    python scripts/bench/multihop.py prepare DATA_DIR WORK_DIR      # corpus files + config
    anchor --config WORK_DIR/<name>/anchor.toml ingest              # for each dataset
    python scripts/bench/multihop.py run DATA_DIR WORK_DIR --anchor target/release/anchor

`run` also scores a plain BM25 over the same passages, as a check that the harness reproduces
the published BM25 figures. Only the Python standard library is needed.
"""

import argparse
import json
import math
import os
import re
import subprocess
import sys
import time
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

DATASETS = ["musique", "2wikimultihopqa", "hotpotqa"]


def load(data_dir, name):
    questions = json.load(open(Path(data_dir) / f"{name}.json"))
    corpus = json.load(open(Path(data_dir) / f"{name}_corpus.json"))
    return questions, corpus


def gold_passages(name, question, corpus_index, title_index):
    """Indices of the corpus passages that support the question."""
    if name == "musique":
        gold = set()
        for p in question["paragraphs"]:
            if p["is_supporting"]:
                gold |= corpus_index[(p["title"], p["paragraph_text"])]
        return gold
    titles = {fact[0] for fact in question["supporting_facts"]}
    gold = set()
    for title in titles:
        gold |= title_index[title]
    return gold


def indexes(corpus):
    by_text, by_title = defaultdict(set), defaultdict(set)
    for i, passage in enumerate(corpus):
        by_text[(passage["title"], passage["text"])].add(i)
        by_title[passage["title"]].add(i)
    return by_text, by_title


def file_name(i):
    return f"p{i:05d}.md"


def prepare(args):
    for name in DATASETS:
        _, corpus = load(args.data, name)
        root = Path(args.work) / name
        pages = root / "corpus"
        pages.mkdir(parents=True, exist_ok=True)
        for i, passage in enumerate(corpus):
            (pages / file_name(i)).write_text(f"# {passage['title']}\n\n{passage['text']}\n")
        config = root / "anchor.toml"
        if not config.exists():
            config.write_text(
                f'data_dir = "{(root / "data").resolve()}"\n\n'
                "[[sources]]\n"
                f'id = "{name}"\n'
                'kind = "wiki"\n'
                f'location = "{pages.resolve()}"\n'
                'trust = "source_of_truth"\n\n'
                "# No model anywhere: questions are classified by heuristics.\n"
                "[models.system1]\n"
                'provider = "disabled"\n'
            )
        print(f"{name}: {len(corpus)} passages in {pages}")


def anchor_ranking(binary, config, question):
    """Passage indices in the order Anchor returns them (retrieval only, no model)."""
    out = subprocess.run(
        [binary, "--config", str(config), "query", "--json", "--retrieval-only", question],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    response = json.loads(out)
    ranked, seen = [], set()
    for item in response["context"]["items"]:
        match = re.search(r"p(\d{5})(?:\.md)?$", item["node"]["key"])
        if match:
            i = int(match.group(1))
            if i not in seen:
                seen.add(i)
                ranked.append(i)
    return ranked


TOKEN = re.compile(r"\w+")


def tokens(text):
    return TOKEN.findall(text.lower())


class BM25:
    """Okapi BM25 (k1 = 1.5, b = 0.75) over title + text, as in the BM25s defaults."""

    def __init__(self, corpus, k1=1.5, b=0.75):
        self.k1, self.b = k1, b
        self.docs = [Counter(tokens(p["title"] + " " + p["text"])) for p in corpus]
        self.lengths = [sum(d.values()) for d in self.docs]
        self.avg = sum(self.lengths) / len(self.lengths)
        df = Counter()
        for d in self.docs:
            df.update(d.keys())
        n = len(self.docs)
        self.idf = {t: math.log(1 + (n - f + 0.5) / (f + 0.5)) for t, f in df.items()}
        self.postings = defaultdict(list)
        for i, d in enumerate(self.docs):
            for t, f in d.items():
                self.postings[t].append((i, f))

    def rank(self, query, k):
        scores = defaultdict(float)
        for t in set(tokens(query)):
            idf = self.idf.get(t)
            if idf is None:
                continue
            for i, f in self.postings[t]:
                norm = f * (self.k1 + 1) / (f + self.k1 * (1 - self.b + self.b * self.lengths[i] / self.avg))
                scores[i] += idf * norm
        return [i for i, _ in sorted(scores.items(), key=lambda x: -x[1])[:k]]


def recall(ranked, gold, k):
    return len(set(ranked[:k]) & gold) / len(gold) if gold else 0.0


def run(args):
    results = {}
    for name in args.datasets:
        questions, corpus = load(args.data, name)
        questions = questions[args.offset :]
        questions = questions[: args.limit] if args.limit else questions
        by_text, by_title = indexes(corpus)
        golds = [gold_passages(name, q, by_text, by_title) for q in questions]
        config = Path(args.work) / name / args.config_name

        started = time.time()
        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            anchor = list(pool.map(lambda q: anchor_ranking(args.anchor, config, q["question"]), questions))
        anchor_seconds = time.time() - started

        systems = [("anchor", anchor)]
        if not args.no_bm25:
            bm25 = BM25(corpus)
            systems.append(("bm25", [bm25.rank(q["question"], 10) for q in questions]))

        row = {}
        for system, rankings in systems:
            row[system] = {
                f"recall@{k}": round(100 * sum(recall(r, g, k) for r, g in zip(rankings, golds)) / len(golds), 1)
                for k in (2, 5, 10)
            }
        row["anchor"]["empty"] = sum(1 for r in anchor if not r)
        row["anchor"]["seconds"] = round(anchor_seconds, 1)
        row["questions"] = len(questions)
        results[name] = row
        print(name, json.dumps(row), flush=True)
        with open(Path(args.work) / name / "rankings.jsonl", "w") as out:
            for q, r, g in zip(questions, anchor, golds):
                out.write(json.dumps({"id": q.get("id") or q.get("_id"), "anchor": r[:10], "gold": sorted(g)}) + "\n")
    json.dump(results, open(Path(args.work) / "scores.json", "w"), indent=2)
    failed = []
    for requirement in args.require:
        target, minimum = requirement.split("=")
        name, metric = target.split(":")
        score = results.get(name, {}).get("anchor", {}).get(metric)
        if score is None or score < float(minimum):
            failed.append(f"{name} {metric} {score} < {minimum}")
    if failed:
        print("below the required scores:\n  " + "\n  ".join(failed), file=sys.stderr)
        return 1
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("prepare")
    p.add_argument("data")
    p.add_argument("work")
    r = sub.add_parser("run")
    r.add_argument("data")
    r.add_argument("work")
    r.add_argument("--anchor", default="anchor")
    r.add_argument("--limit", type=int, default=0, help="first N questions per dataset (0: all)")
    r.add_argument("--offset", type=int, default=0, help="skip the first N questions (e.g. those used for tuning)")
    r.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    r.add_argument("--datasets", nargs="+", choices=DATASETS, default=DATASETS)
    r.add_argument("--config-name", default="anchor.toml", help="configuration file in each dataset's directory")
    r.add_argument("--no-bm25", action="store_true", help="score Anchor only")
    r.add_argument(
        "--require",
        action="append",
        default=[],
        metavar="DATASET:METRIC=MIN",
        help="exit 1 if Anchor scores below MIN (e.g. hotpotqa:recall@5=76); repeatable",
    )
    args = parser.parse_args()
    return {"prepare": prepare, "run": run}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
