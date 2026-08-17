# What is Dr Strange?

Dr Strange is an **AI-native embedded graph database**, written in Rust. This
sentence makes three claims, examined in turn below: it stores a **graph**, it
is **embedded**, and it is **AI-native** — designed for AI workloads from the
outset rather than retrofitted.

## The data model: a property graph, in planes

Dr Strange stores a **property graph**:

- **Nodes** are the entities — a person, a document, a product. Every node has a
  set of **labels** (its types, e.g. `Person`, `Doc`) and a free-form map of
  **properties**.
- **Edges** are directed, typed relationships between nodes — `(alice)
  -[:KNOWS]-> (bob)`. Edges carry properties too.
- **Properties** are typed values: strings, numbers, booleans, byte blobs,
  lists, nested maps — and, crucially, **vectors** (embeddings), which are a
  first-class value type, not an afterthought.

Nodes can carry a caller-supplied **external key** (a stable, human-meaningful
id, unique within its plane) alongside the engine's internal numeric id.

Data is organized into **planes** — independent, named graph namespaces that
live in one database file. A plane is like a schema or a namespace: you might
keep one graph per project, per tenant, or per experiment. Ids are globally
unique across the whole database, but **every query runs in the context of one
plane**, so planes remain cleanly separated. A new database always contains a
`startup` plane.

## The AI-native thesis

Most databases that add AI capabilities pair a conventional engine with a vector
column and a call to an external model. Dr Strange inverts this arrangement: the
capabilities an AI application requires are core engine features:

- **Embeddings are first-class.** A vector is a property value like any other,
  indexed natively with HNSW, so similarity search is fast and lives *inside*
  the query engine.
- **Retrieval fuses signals.** Vector similarity, BM25 keyword relevance, and
  graph proximity combine into a single ranked result — the core of the GraphRAG
  pattern.
- **The engine talks to agents.** Ask a question in plain language and get a
  query plan back, run it, and receive a connected subgraph. Subscribe to a
  plane and receive mutations as they commit. Read the graph *as of* any past
  point.

### A concrete example: GraphRAG

Suppose you have ingested a corpus of documents as a graph — `Doc` nodes with
an `embedding` property, linked by `CITES` and `MENTIONS` edges to `Concept`
nodes. A GraphRAG lookup is a single query:

```text
SEARCH (d:Doc) ON embedding NEAR "how does time-travel work" TOPK 5 RETURN d
```

This embeds the question, retrieves the five most similar documents, and —
because the results are graph nodes — traversal continues directly from them,
into the concepts they mention and the documents that cite them, assembling
grounded context beyond the reach of a standalone vector store.

## Where Dr Strange fits

- **Like SQLite**, it is embedded: a library and a single database on disk, with
  no server to run. You can also start a server (`drsg serve`) when you want a
  dashboard, an HTTP/WebSocket API, and multi-client access.
- **Unlike a classic graph database** (e.g. Neo4j), vectors and hybrid retrieval
  are built into the core, and the surface is designed around agents — natural
  language, change feeds, time-travel.
- **Unlike a pure vector database**, the unit of storage is a *graph*: you get
  relationships, traversal, and graph algorithms, not just nearest neighbors.

For applications built around a knowledge graph, a GraphRAG pipeline, or an
agent's long-term memory, Dr Strange is intended to serve as the single store
for all of it.

## Feature tour

| Capability | What it gives you |
|---|---|
| **Planes** | Many independent graphs in one database |
| **First-class embeddings** | Vector properties, natively HNSW-indexed |
| **Hybrid retrieval** | Fused vector + keyword (BM25) + graph-proximity search |
| **Query language** | A serializable logical plan and an openCypher subset |
| **Graph algorithms** | PageRank, connected components, shortest path, Louvain |
| **Natural-language query** | Ask in plain language → plan → run |
| **Time-travel** | Read the graph *as of* a past commit or timestamp |
| **Change feed** | Subscribe to a plane and receive mutations live |
| **Code digestion** | Sandboxed wasm parser plugins turn a repository into a resolved call graph (8 official languages) |
| **Commit-synced watch** | `serve watch` folds every commit into the plane |
| **Agent tools** | `context` · `search` · `describe` · `grep` · `trace` · `impact` · `snippet` — one round trip each |
| **Backup / restore** | Consistent, id-faithful whole-database snapshots |
| **Interfaces** | Web UI, six language SDKs, a CLI, and an MCP server |

## What v1.0 is

v1.0 is a complete, embedded, AI-native graph database with all of the above,
one storage engine (a hand-rolled LSM, with a legacy alternative), and a stable
JSON-RPC wire protocol that the SDKs are generated from. The chapters that
follow show you how to use each part.

> **Reading in another language?** 中文版见 [Chinese edition](../../zh/book/index.html)。
