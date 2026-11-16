"""Shared pieces of the comparator runners: the neutral question and result files.

Layout of a comparison directory (written by `anchor compare DIR --truth ...`):

    questions.jsonl          {"id", "category", "kind", "question", "expected", "stale", "phrases"}
    results/<system>.jsonl   {"id", "contexts": [text, ...], "answer", "generated_answer", "latency_ms"}

A runner indexes the corpus with its own loader and splitter, answers every question with its
retriever, and writes one results file per retrieval mode. Scoring is done by `anchor compare DIR`.
"""

import argparse
import json
import pathlib
import time

TOP_K = 12
CONTEXT_TOKENS = 6000  # the scorer's budget; four characters per token
MAX_CONTEXTS = 50


def arguments(description):
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("corpus", type=pathlib.Path, help="directory holding the source documents")
    parser.add_argument("run", type=pathlib.Path, help="comparison directory (questions.jsonl, results/)")
    parser.add_argument("--ollama", default="http://127.0.0.1:11434", help="Ollama base URL")
    parser.add_argument("--embedding-model", default="nomic-embed-text")
    parser.add_argument("--limit", type=int, default=0, help="only the first N questions (smoke test)")
    parser.add_argument("--index-dir", type=pathlib.Path, help="where to persist the index (measured for the footprint)")
    return parser


def questions(run, limit=0):
    rows = [json.loads(line) for line in (run / "questions.jsonl").read_text().splitlines() if line.strip()]
    return rows[:limit] if limit else rows


def write_results(run, system, rows):
    path = run / "results" / f"{system}.jsonl"
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():  # keep answers already generated for unchanged contexts
        old = {r["id"]: r for r in map(json.loads, path.read_text().splitlines()) if r}
        for row in rows:
            previous = old.get(row["id"])
            if previous and previous.get("contexts") == row["contexts"]:
                row["generated_answer"] = previous.get("generated_answer")
    path.write_text("".join(json.dumps(row) + "\n" for row in rows))
    print(f"wrote {len(rows)} results to {path}")


def answer_all(rows, retrieve):
    """Runs `retrieve(question) -> [text]` for every question, timing each call."""
    out = []
    for row in rows:
        started = time.perf_counter()
        contexts = retrieve(row["question"])
        out.append({
            "id": row["id"],
            "contexts": contexts,
            "answer": None,
            "generated_answer": None,
            "latency_ms": (time.perf_counter() - started) * 1e3,
        })
    return out


def budgeted(contexts):
    """The contexts the scorer will read: the same cut, so the generator sees the same text."""
    budget = CONTEXT_TOKENS * 4
    kept, size = [], 0
    for context in contexts[:MAX_CONTEXTS]:
        if not kept and len(context) > budget:
            return [context[:budget]]
        if size + len(context) > budget:
            break
        kept.append(context)
        size += len(context) + 2
    return kept


def directory_bytes(path):
    return sum(p.stat().st_size for p in pathlib.Path(path).rglob("*") if p.is_file())


def write_footprint(run, systems, index_dir, index_seconds, note):
    """Records index size on disk, this process's peak memory and indexing time."""
    import resource
    meta = {
        "index_bytes": directory_bytes(index_dir),
        "peak_rss_bytes": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024,
        "index_seconds": index_seconds,
        "note": note,
    }
    for system in systems:
        (run / "results" / f"{system}.meta.json").write_text(json.dumps(meta, indent=1))
    print(f"footprint: index {meta['index_bytes'] / 1e6:.1f} MB, peak RSS {meta['peak_rss_bytes'] / 1e6:.0f} MB")
