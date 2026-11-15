"""LightRAG (graph RAG: an LLM extracts entities and relations at indexing time; queries combine
the extracted graph with vector search). Uses a local Ollama model for extraction and query
keywords. Writes lightrag-mix.jsonl. Indexing is slow (one model call per chunk) but resumable:
the index lives in --workdir and finished documents are skipped on the next run."""

import asyncio
import functools
import pathlib
import time

from lightrag import LightRAG, QueryParam
from lightrag.llm.ollama import ollama_embed, ollama_model_complete
from lightrag.utils import EmbeddingFunc

import common


def documents(corpus, max_docs=0):
    paths = sorted(p for p in corpus.rglob("*") if p.is_file() and not any(part.startswith(".") for part in p.parts))
    out = []
    for path in paths:
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        if text.strip():
            relative = str(path.relative_to(corpus))
            out.append((relative, f"source: {relative}\n\n{text}"))
    return out[:max_docs] if max_docs else out


async def main():
    parser = common.arguments(__doc__)
    parser.add_argument("--workdir", type=pathlib.Path, required=True)
    parser.add_argument("--model", default="llama3.1:8b")
    parser.add_argument("--max-docs", type=int, default=0, help="index only the first N documents (smoke test)")
    parser.add_argument("--index-only", action="store_true")
    args = parser.parse_args()
    args.workdir.mkdir(parents=True, exist_ok=True)

    embed = getattr(ollama_embed, "func", ollama_embed)
    rag = LightRAG(
        working_dir=str(args.workdir),
        llm_model_func=ollama_model_complete,
        llm_model_name=args.model,
        llm_model_max_async=4,
        llm_model_kwargs={"host": args.ollama, "options": {"num_ctx": 8192}, "timeout": 900},
        embedding_func=EmbeddingFunc(
            embedding_dim=768,
            max_token_size=8192,
            func=functools.partial(embed, embed_model=args.embedding_model, host=args.ollama),
        ),
    )
    await rag.initialize_storages()

    docs = documents(args.corpus, args.max_docs)
    started = time.perf_counter()
    batch = 16
    for start in range(0, len(docs), batch):
        part = docs[start:start + batch]
        await rag.ainsert([text for _, text in part], file_paths=[path for path, _ in part])
        print(f"indexed {min(start + batch, len(docs))}/{len(docs)} documents "
              f"({time.perf_counter() - started:.0f} s)", flush=True)
    if args.index_only:
        await rag.finalize_storages()
        return

    param = QueryParam(mode="mix", only_need_context=True, enable_rerank=False,
                       max_total_tokens=common.CONTEXT_TOKENS, max_entity_tokens=2000, max_relation_tokens=2000)
    rows = []
    for row in common.questions(args.run, args.limit):
        began = time.perf_counter()
        context = await rag.aquery(row["question"], param=param)
        rows.append({
            "id": row["id"],
            "contexts": [context] if isinstance(context, str) and context.strip() else [],
            "answer": None,
            "generated_answer": None,
            "latency_ms": (time.perf_counter() - began) * 1e3,
        })
        if len(rows) % 50 == 0:
            print(f"answered {len(rows)} questions", flush=True)
    common.write_results(args.run, "lightrag-mix", rows)
    await rag.finalize_storages()


if __name__ == "__main__":
    asyncio.run(main())
