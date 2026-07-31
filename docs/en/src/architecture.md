# Architecture

This chapter documents the internal architecture. Dr Strange is built in
distinct layers, each with a narrow seam to the next, so the engine can be
embedded, served, or swapped at the storage level without disturbing the layers
above.

## The layers

- **Storage** — a hand-rolled LSM engine (write-ahead log + versioned memtable +
  sorted SST files, with per-SST Bloom filters and a shared block cache) behind
  a backend trait. It provides MVCC: every commit gets a monotonic sequence, and
  a read pins a stable snapshot. This is what makes time-travel, consistent
  snapshots, and coherent caching cheap.
- **Cache** — a per-query and cross-query decoded-object cache, version-stamped
  by commit sequence, so hot reads skip decoding and a write coarsely
  invalidates later snapshots.
- **Computation** — the query executor over a logical plan, plus graph
  algorithms and hybrid-retrieval fusion.
- **API** — the public `Database` / `PlaneHandle` surface: the query builder,
  writes, indexes, time-travel, the change feed, and snapshot/restore.
- **Planes** — the cross-cutting data model that scopes everything.

Above the core sit the wrapper layers: the JSON-RPC web backend + dashboard, the
language SDKs (generated from OpenRPC), the CLI, the MCP server, and the LLM
layer that powers NL query and ingest.

## Design docs

The detailed, per-layer design notes live under [`arch/`][arch] in the
repository — the source of truth this chapter summarizes.

[arch]: https://github.com/wangyingsm/dr-strange/tree/main/arch

## Sections (draft)

- The layer map and the seams between them
- Storage: the LSM engine, WAL/SST format, compaction, MVCC by sequence
- Retention and time-travel; how a historical read is served
- The cache: version stamping and coarse invalidation
- Computation: plan execution, algorithms, hybrid fusion
- Indexes: HNSW and BM25 sidecars, coherence at commit
- The change feed: commit-time observers → WebSocket
- Backends: native LSM vs. redb, and the swap seam
- The wrapper layers and the JSON-RPC / OpenRPC contract
