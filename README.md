<p align="center">
  <img src="crates/dr-strange-web/frontend/public/magic-circle.svg" alt="Dr Strange" width="120" height="120">
</p>

<h1 align="center">Dr Strange</h1>

<p align="center"><em>An AI-native embedded graph database, written in Rust.</em></p>

<p align="center"><strong>English</strong> · <a href="README.zh.md">简体中文</a></p>

📖 **The Dr Strange Book** — the full tutorial and guide:
[English](https://wangyingsm.github.io/dr-strange/en/book/introduction.html) ·
[中文](https://wangyingsm.github.io/dr-strange/zh/book/introduction.html).

## Introduction

Dr Strange is a graph database designed from the outset for AI workloads:
embeddings are a first-class value type, similarity search operates alongside
graph traversal, and the engine exposes primitives suited to agents — natural
language queries, a live change feed, and time-travel — rather than adding AI
features to a conventional graph database after the fact.

Like SQLite, it is **embedded**: a library linked into an application, backed by a
single on-disk database, with no server to operate. Unlike SQLite, it can also
**serve** — `drsg serve` exposes a JSON-RPC 2.0 API, a browser dashboard, and a
WebSocket change feed, with client SDKs in five languages.

For applications built around a knowledge graph, a GraphRAG pipeline, or an
agent's long-term memory, Dr Strange aims to be the single store for all of it.

## Features

| Capability | What it gives you |
|---|---|
| **Planes** | many independent graphs in one database |
| **First-class embeddings** | vector properties, natively HNSW-indexed |
| **Hybrid retrieval** | fused vector + keyword (BM25) + graph-proximity search |
| **Query language** | a serializable logical plan and an openCypher subset |
| **Graph algorithms** | PageRank, connected components, shortest path, Louvain |
| **Natural-language query** | ask in plain language → plan → run |
| **Time-travel** | read the graph *as of* a past commit or timestamp |
| **Change feed** | subscribe to a plane and receive mutations live |
| **Backup / restore** | consistent, id-faithful whole-database snapshots |
| **Interfaces** | a web UI, five language SDKs, a CLI, and an MCP server |

The model-backed features (natural-language query, document ingestion, and
text-embedding search) call an external or local LLM; everything else runs with
no model at all. See [Appendix B](https://wangyingsm.github.io/dr-strange/en/book/appendix-b.html).

## Getting Started

```console
# Build the command-line tool (drsg).
$ cargo build --release -p dr-strange-cli

# Create a plane, add data, and query it.
$ drsg --db graph.drsg plane create social
$ drsg --db graph.drsg cypher --plane social \
    'CREATE (a:Person {name:"Ada"})-[:KNOWS]->(b:Person {name:"Alan"})'

# Serve the dashboard + API.
$ drsg --db graph.drsg serve
```

The full walkthrough — building, the on-disk layout, embeddings and similarity
search, the server and its configuration, and the container image — is in the
book's **Getting Started** chapter:
[English](https://wangyingsm.github.io/dr-strange/en/book/getting-started.html) ·
[中文](https://wangyingsm.github.io/dr-strange/zh/book/getting-started.html).

## Documentation

The book covers each part in depth:
[AI Native](https://wangyingsm.github.io/dr-strange/en/book/ai-native.html) ·
[Query Language](https://wangyingsm.github.io/dr-strange/en/book/query-language.html) ·
[Web UI](https://wangyingsm.github.io/dr-strange/en/book/web-ui.html) ·
[SDK](https://wangyingsm.github.io/dr-strange/en/book/sdk.html) ·
[Embedded CLI](https://wangyingsm.github.io/dr-strange/en/book/embedded-cli.html) ·
[MCP](https://wangyingsm.github.io/dr-strange/en/book/mcp.html) ·
[JSON-RPC API list](https://wangyingsm.github.io/dr-strange/en/book/appendix-a.html).

Build it locally (mdBook): `just docs-serve` (English) or `just docs-serve zh`.

## Architecture

Dr Strange is built in distinct layers — storage (a hand-rolled LSM engine with
MVCC), a version-stamped cache, computation, the API surface, and the
cross-cutting plane model — with the wrapper layers (web, SDKs, CLI, MCP, LLM)
above the core.

- **[Architecture chapter](https://wangyingsm.github.io/dr-strange/en/book/architecture.html)** — the layer map and how
  the commit sequence unifies MVCC, caching, time-travel, and the change feed.
- **[`arch/`](arch/)** — the detailed, per-layer design notes:
  [overview](arch/00-overview.md),
  [storage](arch/01-storage.md),
  [cache](arch/02-cache.md),
  [computation](arch/03-computation.md),
  [API](arch/04-api.md),
  [planes](arch/09-planes.md).

## Benchmarks

A cross-engine comparison against an embedded graph DB (Kùzu), the universal
embedded baseline (SQLite), and the industry-standard server (Neo4j). Every
engine loads the **same** deterministic dataset — 100 K nodes, 500 K edges,
128-dim vectors — and runs the **same** query sets on its own optimal path.

| Operation (median latency, ↓ better) | dr-strange | Kùzu | SQLite | Neo4j |
|---|---|---|---|---|
| Point lookup by key | **3.4 µs** | 397.6 µs | 5.5 µs | 978.6 µs |
| 1-hop expansion | **6.7 µs** | 2.37 ms | 13.7 µs | 799.5 µs |
| 2-hop reachable set | **37.0 µs** | 9.84 ms | 94.7 µs | 1.56 ms |
| Vector top-k query | **387.7 µs** | 10.39 ms | — | 3.57 ms |

The embedded KV design delivers microsecond point and graph queries and a vector
top-k below both Kùzu and Neo4j; bulk load still trails the mature columnar
engines. Numbers are single-run, warm, on one machine — **indicative, not a
leaderboard**. Methodology, caveats, the load-throughput figures, and how to
re-run (`just bench-compare`) are in **[BENCHMARKS.md](BENCHMARKS.md)**.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
