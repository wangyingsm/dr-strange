# What is Dr Strange?

Dr Strange is an **AI-native embedded graph database**. It stores a property
graph — nodes and directed, typed edges, each carrying free-form properties —
and treats the things AI applications need as core features rather than
add-ons: vector embeddings, similarity search fused with traversal,
natural-language querying, a live change feed, historical reads, and
whole-database snapshots.

## AI-native, not AI-bolted-on

A classic graph database can store a vector in a property and call an external
service to search it. Dr Strange is built the other way around:

- **Embeddings are a first-class value type.** A vector lives on a node or edge
  like any other property and is indexed natively (HNSW), so one query can say
  "find the nodes most similar to *X*, then expand two hops out".
- **Hybrid retrieval is built in.** Vector similarity, BM25 keyword relevance,
  and graph proximity are fused into a single ranked result.
- **The engine speaks to agents.** Ask a question in plain language and the
  database turns it into a query and runs it; subscribe to a plane and receive
  mutations as they commit; read the graph *as of* any past commit.

## Embedded first, serving when you need it

- **Embedded:** link the `dr-strange-core` crate and open a database file — no
  server, no daemon, like SQLite.
- **Serving:** run `drsg serve` to expose a JSON-RPC 2.0 API, a web dashboard,
  and a WebSocket change feed, reachable from the language SDKs, the MCP server
  for LLM agents, or plain `curl`.

## Planes: many graphs in one database

Data is organized into **planes** — independent, named graph namespaces in one
database file (think schemas or namespaces). Node and edge ids are globally
unique, but every query runs in the context of one plane.

## Sections (draft)

- What a property graph is here (nodes, typed edges, properties, planes)
- The "AI-native" thesis, with a concrete GraphRAG example
- Where Dr Strange fits (vs. SQLite, vs. Neo4j, vs. a vector DB)
- The feature tour: embeddings · hybrid retrieval · NL query · graph
  algorithms · time-travel · change feed · backup
- What v1.0 does and does not promise
