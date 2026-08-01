# Architecture

This chapter documents the internal architecture. Dr Strange is built in
distinct layers, each with a narrow seam to the next, so the engine can be
embedded, served, or swapped at the storage level without disturbing the layers
above.

## The layers

| Layer | Responsibility |
|---|---|
| **Storage** | durable key/value with MVCC snapshots, behind a backend trait |
| **Cache** | decoded-object memoization, version-stamped by commit sequence |
| **Computation** | plan execution, graph algorithms, hybrid-retrieval fusion |
| **API** | the `Database` / `PlaneHandle` surface: queries, writes, indexes, time-travel, the change feed, snapshots |
| **Planes** | the cross-cutting namespace that scopes all data |

Above the core sit the wrapper layers: the JSON-RPC web backend and dashboard,
the language SDKs, the CLI, the MCP server, and the LLM layer that powers
natural-language query and ingestion.

## The commit sequence

One primitive threads through every layer: the **commit sequence** — a
monotonic number assigned to each committed write. It is the storage engine's
MVCC version, the cache's coherence stamp, the freshness stamp on the index
sidecars, the address for a time-travel read, and the sequence carried by every
change event. A single, engine-wide clock keeps the layers consistent without
separate coordination.

## Storage

The graph layer is written against a backend trait — `begin_read` yields a
stable MVCC snapshot, `begin_write` a serialized single-writer transaction — over
a fixed set of logical tables (nodes, edges, forward/reverse adjacency, label and
property indexes, external keys, plane metadata). Keys are big-endian, so byte
order is scan order and every per-plane or per-node range is a contiguous prefix.

The default backend is a hand-rolled **LSM engine**:

- A write is appended to a **write-ahead log** — a length-prefixed, CRC32-checked
  batch record, `fsync`ed before the change is published — and inserted into a
  versioned in-memory **memtable** stamped with the commit sequence.
- When the memtable exceeds a threshold it is flushed to an immutable, sorted
  **SST** file (with a per-SST Bloom filter), and the WAL is rotated.
- A read merges the memtable over the SSTs, newest run first, returning the first
  version at or below its snapshot.
- **Compaction** merges accumulated runs into one, reclaiming versions no reader
  can still observe.

Because each version carries its commit sequence and SSTs are immutable, a
reader holds no locks across its lifetime, and an old snapshot's data remains
reachable until compaction reclaims it.

## Time-travel and retention

Time-travel falls out of the MVCC design almost for free. `begin_read_at(seq)`
pins a read to any past commit sequence, and the read path — which already
returns versions at or below a snapshot — serves it unchanged. The only
requirement is that the historical versions survive compaction, which a
**retention window** guarantees by flooring the compaction GC (unbounded by
default; a bounded window trades history for disk). A timestamp address resolves
to a commit by binary-searching a wall-clock time recorded on each commit. Only
the native backend retains prior versions, so time-travel is native-only.

## The cache

The executor never touches storage directly; it reads through a cache with two
tiers. A per-query **L1** memoizes decoded nodes, edges, and adjacency for the
life of one query (bound to its snapshot). A cross-query **L2** — a bounded,
version-stamped object cache — is shared by every query: an entry is served only
when its stamp equals the reader's commit sequence, so any write silently
invalidates prior entries for later snapshots. This exact-sequence rule also
makes a historical read cache-safe: it can never be served a newer version.

## Computation

The executor runs a logical plan over a stream of rows, each carrying a current
node and the path that produced it, so a read can return the connected subgraph
rather than bare endpoints. Graph algorithms run over an index-dense frame of the
plane's adjacency, read-only over one snapshot. Hybrid retrieval gathers each
channel independently, normalizes within-channel scores, and fuses them by
weighted sum, with the graph channel seeded from the strongest hits.

## Indexes

The vector (HNSW) and keyword (BM25) indexes are in-memory registries, their
declarations recorded in storage. On open, a registry loads from its sidecar
(`.hnsw` / `.bm25`) when the sidecar's stamped commit sequence matches the data,
and otherwise rebuilds from the key/value store — which is always the source of
truth. Coherence is maintained at commit: a write transaction buffers the index
changes it implies and applies them to the registries under a write lock once the
data is durable. A snapshot serializes the live registries to sidecar bytes, and
an id-faithful restore reloads them without a rebuild.

## The change feed

A `Database` accepts one commit-time observer. A write transaction buffers the
node and edge mutations it makes; at commit, once the data is durable, they are
collapsed per entity, resolved to records at the committed snapshot (with
embeddings and internal properties stripped), and handed to the observer. The web
layer registers an observer that publishes each change set into a broadcast
channel, which every `plane.watch` WebSocket subscriber drains — delivery is
best-effort, so a slow consumer drops overflow rather than stalling writers.

## Backends

The storage backend is a compile-time choice: the native LSM engine (default),
the legacy redb engine, or an in-memory engine for tests. Capabilities that
depend on prior versions — time-travel, and its retention — exist only on the
native backend and are feature-gated accordingly; the read seam compiles on every
backend regardless.

## The wire contract

The wrapper layers speak **JSON-RPC 2.0**. The `drsg serve` backend, the MCP
server, and the SDKs all use it, and the surface is described by an **OpenRPC**
schema that is the single source of truth: the SDKs are generated from it, the
server returns it from `rpc.discover`, and drift tests fail if the schema, the
dispatch table, and the generated clients ever disagree. The change-feed
subscription is a WebSocket extension of the same protocol.

## Design documents

The detailed, per-layer design notes live under [`arch/`][arch] in the
repository — the source of truth this chapter summarizes.

[arch]: https://github.com/wangyingsm/dr-strange/tree/main/arch
