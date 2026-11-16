"""Pairwise LLM-judged comparison, as LightRAG evaluates itself (docs/Reproduce.md in its repo):
a judge model sees a question and two answers and picks the better one for Comprehensiveness,
Diversity and Empowerment, then an overall winner. Reported as win rates.

Compares the generated answers (see generate.py) of one system against each of the others.
Which answer is shown first alternates from question to question to cancel position bias.
Writes judge/<system>-vs-<other>.json into the comparison directory."""

import argparse
import concurrent.futures
import json
import pathlib
import urllib.request

CRITERIA = ["Comprehensiveness", "Diversity", "Empowerment", "Overall Winner"]

PROMPT = """---Role---
You are an expert tasked with evaluating two answers to the same question based on three criteria: **Comprehensiveness**, **Diversity**, and **Empowerment**.

---Goal---
You will evaluate two answers to the same question based on three criteria: **Comprehensiveness**, **Diversity**, and **Empowerment**.

- **Comprehensiveness**: How much detail does the answer provide to cover all aspects and details of the question?
- **Diversity**: How varied and rich is the answer in providing different perspectives and insights on the question?
- **Empowerment**: How well does the answer help the reader understand and make informed judgments about the topic?

For each criterion, choose the better answer (either Answer 1 or Answer 2) and explain why. Then, select an overall winner based on these three categories.

Here is the question:
{query}

Here are the two answers:

**Answer 1:**
{answer1}

**Answer 2:**
{answer2}

Evaluate both answers using the three criteria listed above and provide detailed explanations for each criterion.

Output your evaluation in the following JSON format:

{{
    "Comprehensiveness": {{"Winner": "[Answer 1 or Answer 2]", "Explanation": "[Provide explanation here]"}},
    "Diversity": {{"Winner": "[Answer 1 or Answer 2]", "Explanation": "[Provide explanation here]"}},
    "Empowerment": {{"Winner": "[Answer 1 or Answer 2]", "Explanation": "[Provide explanation here]"}},
    "Overall Winner": {{"Winner": "[Answer 1 or Answer 2]", "Explanation": "[Summarize why this answer is the overall winner based on the three criteria]"}}
}}"""


def ask(base, model, prompt):
    body = {"model": model, "stream": False, "format": "json",
            "messages": [{"role": "user", "content": prompt}],
            "options": {"temperature": 0, "num_ctx": 8192, "num_predict": 500}}
    request = urllib.request.Request(
        f"{base}/api/chat", data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=900) as response:
        return json.load(response)["message"]["content"]


def load(run, system):
    rows = map(json.loads, (run / "results" / f"{system}.jsonl").read_text().splitlines())
    return {r["id"]: r["generated_answer"] for r in rows if r.get("generated_answer")}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=pathlib.Path)
    parser.add_argument("--system", default="anchor")
    parser.add_argument("--against", nargs="+", required=True)
    parser.add_argument("--ollama", default="http://127.0.0.1:11434")
    parser.add_argument("--model", default="llama3.1:8b")
    parser.add_argument("--workers", type=int, default=4)
    args = parser.parse_args()

    questions = {q["id"]: q["question"] for q in map(json.loads, (args.run / "questions.jsonl").read_text().splitlines())}
    ours = load(args.run, args.system)
    (args.run / "judge").mkdir(exist_ok=True)
    for other in args.against:
        theirs = load(args.run, other)
        ids = sorted(set(ours) & set(theirs))

        def verdict(item):
            index, qid = item
            first_is_ours = index % 2 == 0
            a1, a2 = (ours[qid], theirs[qid]) if first_is_ours else (theirs[qid], ours[qid])
            try:
                parsed = json.loads(ask(args.ollama, args.model, PROMPT.format(query=questions[qid], answer1=a1, answer2=a2)))
            except (json.JSONDecodeError, OSError):
                return {}
            out = {}
            for criterion in CRITERIA:
                winner = str((parsed.get(criterion) or {}).get("Winner", "")) if isinstance(parsed.get(criterion), dict) else ""
                if "1" in winner and "2" not in winner:
                    out[criterion] = first_is_ours
                elif "2" in winner and "1" not in winner:
                    out[criterion] = not first_is_ours
            return out

        with concurrent.futures.ThreadPoolExecutor(args.workers) as pool:
            verdicts = list(pool.map(verdict, enumerate(ids)))
        summary = {"system": args.system, "against": other, "questions": len(ids), "judge": args.model, "win_rate": {}}
        for criterion in CRITERIA:
            decided = [v[criterion] for v in verdicts if criterion in v]
            summary["win_rate"][criterion] = {
                "decided": len(decided),
                args.system: round(sum(decided) / len(decided), 3) if decided else None,
            }
        (args.run / "judge" / f"{args.system}-vs-{other}.json").write_text(json.dumps(summary, indent=1))
        print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()
