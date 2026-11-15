"""Writes an answer for each system's retrieved contexts with one shared model, so that systems
are compared on what a reader model can actually conclude from what they retrieved.

Every system gets the same model, prompt, context budget and sample of questions. Answers are
stored as `generated_answer` in results/<system>.jsonl and scored by `anchor compare DIR`.
Rows that already have an answer are skipped, so the script can be re-run or extended."""

import argparse
import concurrent.futures
import json
import pathlib
import urllib.request

import common

PROMPT = """Answer the question using only the context below. Be brief and specific: name the \
team, person, service, portfolio or domain asked for, or quote the stated reason. If the context \
contains conflicting claims, prefer the one the context marks as more authoritative. If the \
context does not contain the answer, reply "unknown".

Context:
{context}

Question: {question}
Answer:"""


def sample(questions, per_category):
    """Every question with a stale claim, plus an evenly spaced sample of each category."""
    chosen = {q["id"] for q in questions if q.get("stale")}
    for category in sorted({q["category"] for q in questions}):
        rows = [q for q in questions if q["category"] == category]
        step = max(1, len(rows) // per_category)
        chosen.update(q["id"] for q in rows[::step][:per_category])
    return chosen


def generate(base, model, question, contexts):
    body = {
        "model": model,
        "stream": False,
        "messages": [{"role": "user", "content": PROMPT.format(
            context="\n\n---\n\n".join(common.budgeted(contexts)) or "(nothing retrieved)", question=question)}],
        "options": {"temperature": 0, "num_ctx": 8192, "num_predict": 160},
    }
    request = urllib.request.Request(
        f"{base}/api/chat", data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=600) as response:
        return json.load(response)["message"]["content"].strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=pathlib.Path)
    parser.add_argument("--ollama", default="http://127.0.0.1:11434")
    parser.add_argument("--model", default="llama3.1:8b")
    parser.add_argument("--per-category", type=int, default=50)
    parser.add_argument("--systems", nargs="*", help="result file stems (default: all)")
    parser.add_argument("--workers", type=int, default=4)
    args = parser.parse_args()

    questions = {q["id"]: q for q in common.questions(args.run)}
    chosen = sample(list(questions.values()), args.per_category)
    files = sorted((args.run / "results").glob("*.jsonl"))
    if args.systems:  # in the order given
        files = [f for name in args.systems for f in files if f.stem == name]
    with concurrent.futures.ThreadPoolExecutor(args.workers) as pool:
        for path in files:
            rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
            todo = [r for r in rows if r["id"] in chosen and not r.get("generated_answer")]
            answers = pool.map(
                lambda r: generate(args.ollama, args.model, questions[r["id"]]["question"], r["contexts"]), todo)
            for row, answer in zip(todo, answers):
                row["generated_answer"] = answer
            path.write_text("".join(json.dumps(row) + "\n" for row in rows))
            print(f"{path.stem}: generated {len(todo)} answers", flush=True)


if __name__ == "__main__":
    main()
