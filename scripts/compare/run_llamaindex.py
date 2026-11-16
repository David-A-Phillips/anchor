"""LlamaIndex with its defaults: SimpleDirectoryReader, the default sentence splitter, a vector
index, BM25, and reciprocal-rank fusion of both. Writes llamaindex-{vector,bm25,hybrid}.jsonl."""

import time

from llama_index.core import Settings, SimpleDirectoryReader, VectorStoreIndex
from llama_index.core.retrievers import QueryFusionRetriever
from llama_index.core.schema import MetadataMode
from llama_index.embeddings.ollama import OllamaEmbedding
from llama_index.retrievers.bm25 import BM25Retriever

import common


def main():
    args = common.arguments(__doc__).parse_args()
    Settings.embed_model = OllamaEmbedding(model_name=args.embedding_model, base_url=args.ollama)
    Settings.llm = None
    started = time.perf_counter()
    documents = SimpleDirectoryReader(str(args.corpus), recursive=True).load_data()
    index = VectorStoreIndex.from_documents(documents)
    nodes = list(index.docstore.docs.values())
    index_seconds = time.perf_counter() - started
    print(f"indexed {len(documents)} documents as {len(nodes)} chunks in {index_seconds:.1f} s")

    vector = index.as_retriever(similarity_top_k=common.TOP_K)
    bm25 = BM25Retriever.from_defaults(nodes=nodes, similarity_top_k=common.TOP_K)
    hybrid = QueryFusionRetriever(
        [vector, bm25], similarity_top_k=common.TOP_K, num_queries=1, mode="reciprocal_rerank", use_async=False
    )
    rows = common.questions(args.run, args.limit)
    for name, retriever in (("vector", vector), ("bm25", bm25), ("hybrid", hybrid)):
        def retrieve(question, retriever=retriever):
            return [hit.node.get_content(metadata_mode=MetadataMode.LLM) for hit in retriever.retrieve(question)]
        common.write_results(args.run, f"llamaindex-{name}", common.answer_all(rows, retrieve))
    if args.index_dir:
        index.storage_context.persist(persist_dir=str(args.index_dir / "vector"))
        bm25.persist(str(args.index_dir / "bm25"))
        common.write_footprint(
            args.run, ["llamaindex-vector", "llamaindex-bm25", "llamaindex-hybrid"], args.index_dir, index_seconds,
            "vector + BM25 index together; embedding model runs in Ollama (not counted: 274 MB on disk)")


if __name__ == "__main__":
    main()
