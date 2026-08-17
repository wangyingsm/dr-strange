# dr-strange — Architecture Overview

An AI-native embedded graph database, written in Rust.

**Status**: living design notes, kept current with the code · begun 2026-07-22

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

The "v2" items in this table have since shipped: the openCypher-subset
language (`drsg cypher`, the `dr-strange-parser` crate) and the native LSM
engine that replaced the bootstrap KV behind the same `StorageEngine` trait.

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
│  │ KV backend (native LSM)  │ Vector index (HNSW)      │  │
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
    dr-strange-mcp/        # MCP server (stdio; also mounted at /mcp by web)
    dr-strange-llm/        # document reading, embeddings, digest, NL→plan
    dr-strange-parser/     # the openCypher-subset query language
    dr-strange-web/        # `drsg serve` — JSON-RPC API + dashboard
    dr-strange-log/        # the binaries' tracing subscriber
  sdk/             # client SDKs (six languages)
  benchmarks/      # the cross-engine comparison harness
  arch/            # these documents
  docs/            # the tutorial book (en + zh)
```

`dr-strange-core` may split (`dr-storage`, `dr-plan`) later if compile times or team
boundaries demand it; start unified.

## 5. Milestones

Each milestone ends with a working vertical slice, not a finished layer.

- **M0 — walking skeleton** ✅: workspace layout; `StorageEngine` trait + redb
  backend; the full node/edge/adjacency encoding (both adjacency tables,
  label index, property codec) and a create → get → 1-hop-expand vertical
  slice, round-tripped through a smoke test.
- **M1 — real graph storage** ✅: deletes (node cascades to incident edges,
  edge, plane), external keys (`create_node_with_key` / lookup), property
  mutation (`set_prop`/`remove_prop` on nodes and edges), batched node/edge
  ID allocation; property-based tests against an in-memory model.
- **M2 — query engine v0** ✅: serializable logical plan
  (scan/seek → expand/expand-var/filter/sort/distinct/skip/limit) + total
  `Expr` evaluator + pull-based executor over the `GraphReader` seam; builder
  API with `nodes`/`ids`/`count`/`select` terminals. Row model is the linear
  pipeline (current node + trail). The cache *seam* landed (`UncachedReader`);
  the moka caches the benchmarks were meant to size have since shipped too —
  a per-query `CachedReader` over a persistent, commit-stamped store (arch/02).
- **M3 — AI-native** ✅: `Metric` + exact brute-force + hand-rolled pure-Rust
  HNSW behind `VectorIndex`; native hybrid operators `VectorTopK` /
  `FrontierTopK` / `ExpandBeam` with a row score channel and
  `score`/`hops`/`distance`/`similarity` fusion; a declared-index registry
  (`ensure_vector_index`, rebuilt from the KV on open, write-coherent);
  soft-schema catalog + introspection (`plane.catalog()` / `db.catalog()`).
  Deferred: HNSW graph sidecar (open-time speedup), incremental catalog
  maintenance.
- **M4 — first wrappers** ✅: `drsg` CLI (clap — init/plane/import/export/get/
  query/catalog/index/stats/check) and `drsg-mcp` MCP server (rmcp SDK, stdio,
  10 tools over the core API). Shared JSON dialect in the core's feature-gated
  `json` module. `digest` had its own design session and shipped (arch/07,
  ROADMAP §8).
- **M5 — hardening** ✅: deterministic crash-recovery tests (fault-injecting
  engine — error propagation + commit atomicity; redb reopen restores every
  layer incl. the rebuilt HNSW index, with a reopen proptest); criterion
  micro-benchmarks (insert/lookup/expand, brute-force vs HNSW); API polish
  (plane rename + properties get/set). The benchmarks surfaced and fixed a
  memory-snapshot deep-copy (now `Arc` copy-on-write). Deferred: external-DB
  comparison (Kùzu/Neo4j), and — informed by the benchmarks — building the
  moka `CachedReader` and the HNSW sidecar. Then decide v2: query language,
  custom storage engine, network server.
- **M6 — web UI v1** ✅: `drsg serve` (JSON-RPC 2.0 backend) with dashboard and
  visual graph plots ([08-web-ui.md](08-web-ui.md)).

## 6. Cross-cutting decisions

All four cross-cutting questions this doc opened with are settled; the answers
are recorded here so the reasoning isn't lost. Layer-specific questions live at
the bottom of each layer doc.

1. **On-disk format versioning** — settled. The format version lives in `meta`
   from day one, and `storage/graph/meta.rs` walks a migration ladder at open
   (`migrate_step`), so an older database is carried forward rather than refused
   or, worse, silently misread. Shipped v1.5.0.
2. **Error taxonomy** — settled: one `dr_strange_core::Error` enum
   (`core/src/error.rs`), not per-layer error types.
3. **Async or sync core** — settled: the core is **sync**, as the original
   leaning had it. Async lives only in the wrapper processes (`drsg serve`,
   `drsg-mcp`), which is why every engine entry point is a synchronous closure
   (`with_read` / `with_write`).
4. **Name/branding** — settled: the project is `drsg`, databases carry the
   `.drsg` extension, and the index sidecars carry their own magic bytes
   (`DRSH` for the HNSW graph, `DRSK` for the BM25 index).
