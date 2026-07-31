# AI Native

This chapter covers the features that make Dr Strange a database *for* AI, not
just a database *with* AI features.

## Embeddings as a first-class value

A property whose value is a vector is an embedding. Declare an index on a
`(label, property)` pair and the engine maintains an HNSW index over it, so
similarity search is fast and lives right next to the graph:

```text
SEARCH (d:Doc) ON embedding NEAR "how does time-travel work" TOPK 5 RETURN d
```

The text is embedded server-side (the provider key comes from the server's
environment) and the five nearest documents come back — then you can traverse
from them like any other result.

## Hybrid retrieval

Real retrieval rarely wants one signal. Dr Strange fuses three channels into a
single ranked list:

- **Vector** — embedding similarity,
- **Keyword** — BM25 over a stemmed, language-aware text index,
- **Graph** — proximity to the strongest hits, decayed per hop.

## Natural-language querying

Ask a question in plain language; the engine grounds a language model on the
plane's schema, has it emit a query plan (with a bounded repair loop and
embedding-backed tools to find the right edges and entities), runs it, and
returns the connected subgraph.

## Ingesting documents (AIgest)

Turn unstructured text (Markdown, PDF, DOCX) into graph structure: an LLM
extracts entities and relations, embeds them, links them to what already exists,
and writes the result — the pipeline behind the dashboard's **AIgest** page.

## Sections (draft)

- Vector properties and declaring an HNSW index
- Keyword (BM25) indexes: stemming, language, stopwords
- Hybrid search and how the channels are fused
- `plane.ask`: NL → query plan → run (the agentic loop, repair, tools)
- AIgest: document → entities/relations → embed → link → write
- Providers and keys (OpenAI / DeepSeek / Qwen / Ollama), server-side
- Time-travel and the change feed as agent primitives (cross-links)
