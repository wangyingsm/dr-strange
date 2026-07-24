# dr-strange — Architecture Overview

An AI-native embedded graph database, written in Rust.

**Status**: draft for review · 2026-07-22

Per-layer designs live beside this file; this document holds the vision, the
locked top-level decisions, and everything that spans layers.

| Layer | Doc |
|---|---|
| Storage | [01-storage.md](01-storage.md) |
| Cache | [02-cache.md](02-cache.md) |
| Computation | [03-computation.md](03-computation.md) |
| API (core surface) | [04-api.md](04-api.md) |
| CLI tools | [05-tools.md](05-tools.md) |
| MCP service | [06-mcp.md](06-mcp.md) |
| LLM layer | [07-llm.md](07-llm.md) |
| Web UI | [08-web-ui.md](08-web-ui.md) |
| Planes (cross-cutting data model) | [09-planes.md](09-planes.md) |

## 1. Vision

A graph database designed from day one for AI workloads, rather than a classic
graph database with AI features bolted on:

- **Embeddings are a first-class value type.** Vector properties live on nodes
  and edges, indexed natively, so one query can mix graph traversal with
  similarity search ("find nodes similar to *X*, then expand 2 hops"). This is
  the core GraphRAG use case.
- **MCP is a primary interface, not a wrapper afterthought.** Schema
  introspection, incremental exploration, and token-frugal result formats are
  designed for an LLM consumer.
- **Schema-flexible by default.** LLM-extracted knowledge is messy. Nodes and
  edges carry an open property map that can expand or shrink per record; the
  database observes and reports shape rather than enforcing DDL.

## 2. Decisions locked so far

| Decision | Choice | Rationale |
|---|---|---|
| Storage foundation | Graph layer built on a proven embedded KV, behind a `StorageEngine` trait | Inherit ACID/WAL/recovery; spend effort on the graph encoding and AI features where the product value is. A custom engine can replace the KV in v2 without touching upper layers. |
| Planes | The DB is a pile of **planes** — exclusive graph canvases (partition model); every node/edge lives in exactly one plane | Hard isolation per document/extraction-run/agent-session; cheap plane drop (prefix delete); cross-plane identity via external keys + entity resolution, made explicit rather than implicit. See [09-planes.md](09-planes.md). |
| Data model | Labeled property graph + first-class vectors, **soft schema** | Familiar LPG semantics (Neo4j/Kùzu users, LLM training data), but properties are an open map — no fixed columns, no required DDL. Properties may expand or shrink per record at any time. |
| Deployment shape | Embedded-first (SQLite/DuckDB/Kùzu style) | Core is a Rust library. Server, MCP service, and web UI are thin processes wrapping it. Zero-ops embedding for MCP users; a network server later reuses the same core. |
| Query interface | Programmatic (builder) API in v1; query language in v2 | Ship the execution engine first; a Cypher/GQL-subset parser lands in v2 on top of the same logical plan layer. |
| Wire protocol | **JSON-RPC 2.0** for every remote surface (web UI backend, future network server) | One call convention everywhere — MCP is itself JSON-RPC 2.0, so wrappers share framing, error model, and method-naming; serialized plans/values ride as params verbatim. |

## 3. Layer diagram

```
┌───────────────────────────────────────────────────────────┐
│  Wrapper layers (separate processes / crates)             │
│  ┌──────────┐ ┌─────────────┐ ┌──────────┐ ┌───────────┐  │
│  │ CLI tools│ │ MCP service │ │ LLM layer│ │  Web UI   │  │
│  └────┬─────┘ └──────┬──────┘ └────┬─────┘ └─────┬─────┘  │
└───────┼──────────────┼─────────────┼─────────────┼────────┘
        └──────────────┴──────┬──────┴─────────────┘
┌─────────────────────────────┼─────────────────────────────┐
│  DB core (embedded library) ▼                             │
│  ┌─────────────────────────────────────────────────────┐  │
│  │ API layer — public Rust API, session/txn handles,   │  │
│  │ query builder                                       │  │
│  ├─────────────────────────────────────────────────────┤  │
│  │ Computation layer — logical plan, traversal engine, │  │
│  │ hybrid graph+vector execution, soft-schema catalog  │  │
│  ├─────────────────────────────────────────────────────┤  │
│  │ Cache layer — decoded records, adjacency segments,  │  │
│  │ dictionaries; MVCC snapshot-aware                   │  │
│  ├─────────────────────────────────────────────────────┤  │
│  │ Storage layer — graph encoding over StorageEngine   │  │
│  │ trait; VectorIndex trait; MVCC txns from backend    │  │
│  ├──────────────────────────┬──────────────────────────┤  │
│  │ KV backend (redb, v1)    │ Vector index (HNSW)      │  │
│  └──────────────────────────┴──────────────────────────┘  │
└───────────────────────────────────────────────────────────┘
```

Everything above the core talks to the same public Rust API. The wire server
(when it arrives) is just another wrapper. No wrapper contains database logic.

## 4. Crate layout

Cargo workspace (single repo):

```
dr-strange/
  crates/
    dr-strange-core/       # storage + computation + API layers (the database)
    dr-strange-cli/        # `drsg` binary
    dr-strange-mcp/        # MCP server
    dr-strange-llm/        # embedding/NL helpers (optional dep of mcp/cli)
  arch/            # these documents
```

`dr-strange-core` may split (`dr-storage`, `dr-plan`) later if compile times or team
boundaries demand it; start unified.

## 5. Milestones

Each milestone ends with a working vertical slice, not a finished layer.

- **M0 — walking skeleton**: workspace layout; `StorageEngine` trait + redb
  backend; the full node/edge/adjacency encoding (both adjacency tables,
  label index, property codec) and a create → get → 1-hop-expand vertical
  slice, round-tripped through a smoke test.
- **M1 — real graph storage**: deletes (node cascades to incident edges,
  edge, plane), external keys (`create_node_with_key` / lookup), property
  mutation (`set_prop`/`remove_prop` on nodes and edges), batched node/edge
  ID allocation; property-based tests against an in-memory model.
- **M2 — query engine v0**: logical plan + iterator executor; builder API;
  filters, projections, multi-hop expansion, limits. Cache layer lands here
  (pass-through `NoCache` in M0–M1), sized by traversal benchmarks.
- **M3 — AI-native**: vector property type, HNSW index + `VectorTopK`, hybrid
  query slice ("similar then expand"); soft-schema catalog + introspection.
- **M4 — first wrappers**: `drsg` CLI (import/query/stats; `digest` once its
  design session lands) and MCP server.
- **M5 — hardening**: crash-recovery tests, benchmarks (vs Kùzu/Neo4j on
  LDBC-ish workloads), API polish. Then decide v2: query language, custom
  storage engine, network server.
- **M6 — web UI v1**: `drsg serve` (JSON-RPC 2.0 backend) with dashboard and
  visual graph plots ([08-web-ui.md](08-web-ui.md)).

## 6. Cross-cutting open questions

Layer-specific open questions live at the bottom of each layer doc.

1. **Versioning of on-disk format** — format version in `meta` from day one;
   migration story TBD before first external user.
2. **Error taxonomy** — one `dr_strange_core::Error` enum vs per-layer errors; decide
   at M0.
3. **Async or sync core** — leaning sync core (embedded DBs are CPU/IO-bound,
   redb is sync) with async only in wrapper processes; confirm at M0.
4. **Name/branding** — on-disk file extension `.dr`? Magic bytes? Bikeshed
   later, but magic bytes must land in M0.
