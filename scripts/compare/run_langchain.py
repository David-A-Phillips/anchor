"""LangChain with common defaults: DirectoryLoader + TextLoader, RecursiveCharacterTextSplitter
(1000 characters, 200 overlap), FAISS over Ollama embeddings, BM25, and an ensemble of both
(weighted reciprocal-rank fusion, as EnsembleRetriever does). Writes langchain-{vector,bm25,hybrid}.jsonl."""

import time

from langchain_community.document_loaders import DirectoryLoader, TextLoader
from langchain_community.retrievers import BM25Retriever
from langchain_community.vectorstores import FAISS
from langchain_ollama import OllamaEmbeddings
from langchain_text_splitters import RecursiveCharacterTextSplitter

import common


def text(document, corpus):
    source = document.metadata.get("source", "")
    try:
        source = str(__import__("pathlib").Path(source).resolve().relative_to(corpus.resolve()))
    except ValueError:
        pass
    return f"source: {source}\n\n{document.page_content}"


def fuse(lists, k):
    scores, by_key = {}, {}
    for hits in lists:
        for rank, document in enumerate(hits):
            key = (document.metadata.get("source"), document.page_content)
            by_key[key] = document
            scores[key] = scores.get(key, 0.0) + 1.0 / (60 + rank + 1)
    ranked = sorted(scores, key=scores.get, reverse=True)
    return [by_key[key] for key in ranked[:k]]


def main():
    args = common.arguments(__doc__).parse_args()
    started = time.perf_counter()
    loader = DirectoryLoader(
        str(args.corpus), glob="**/*", loader_cls=TextLoader,
        loader_kwargs={"autodetect_encoding": False, "encoding": "utf-8"}, silent_errors=True,
    )
    documents = loader.load()
    chunks = RecursiveCharacterTextSplitter(chunk_size=1000, chunk_overlap=200).split_documents(documents)
    embeddings = OllamaEmbeddings(model=args.embedding_model, base_url=args.ollama)
    store = FAISS.from_documents(chunks, embeddings)
    print(f"indexed {len(documents)} documents as {len(chunks)} chunks in {time.perf_counter() - started:.1f} s")

    vector = store.as_retriever(search_kwargs={"k": common.TOP_K})
    bm25 = BM25Retriever.from_documents(chunks)
    bm25.k = common.TOP_K
    modes = {
        "vector": lambda q: vector.invoke(q),
        "bm25": lambda q: bm25.invoke(q),
        "hybrid": lambda q: fuse([vector.invoke(q), bm25.invoke(q)], common.TOP_K),
    }
    rows = common.questions(args.run, args.limit)
    for name, mode in modes.items():
        def retrieve(question, mode=mode):
            return [text(document, args.corpus) for document in mode(question)]
        common.write_results(args.run, f"langchain-{name}", common.answer_all(rows, retrieve))


if __name__ == "__main__":
    main()
