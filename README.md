# Dr Strange

> An AI-native embedded graph database, written in Rust.

📖 **The Dr Strange Book** — the full tutorial and guide:
[English](docs/en/src/introduction.md) · [中文](docs/zh/src/introduction.md).

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
no model at all. See [Appendix B](docs/en/src/appendix-b.md).

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
[English](docs/en/src/getting-started.md) · [中文](docs/zh/src/getting-started.md).

## Documentation

The book covers each part in depth:
[AI Native](docs/en/src/ai-native.md) ·
[Query Language](docs/en/src/query-language.md) ·
[Web UI](docs/en/src/web-ui.md) ·
[SDK](docs/en/src/sdk.md) ·
[Embedded CLI](docs/en/src/embedded-cli.md) ·
[MCP](docs/en/src/mcp.md) ·
[JSON-RPC API list](docs/en/src/appendix-a.md).

Build it locally (mdBook): `just docs-serve` (English) or `just docs-serve zh`.

## Architecture

Dr Strange is built in distinct layers — storage (a hand-rolled LSM engine with
MVCC), a version-stamped cache, computation, the API surface, and the
cross-cutting plane model — with the wrapper layers (web, SDKs, CLI, MCP, LLM)
above the core.

- **[Architecture chapter](docs/en/src/architecture.md)** — the layer map and how
  the commit sequence unifies MVCC, caching, time-travel, and the change feed.
- **[`arch/`](arch/)** — the detailed, per-layer design notes:
  [overview](arch/00-overview.md),
  [storage](arch/01-storage.md),
  [cache](arch/02-cache.md),
  [computation](arch/03-computation.md),
  [API](arch/04-api.md),
  [planes](arch/09-planes.md).
