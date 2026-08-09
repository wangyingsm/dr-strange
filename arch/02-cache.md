# Cache Layer

**Status**: draft · seam implemented, caching deferred (M2) · 2026-07-27

**M2 landed the seam, not the cache.** The `GraphReader` trait (§2) and the
pass-through `UncachedReader` are built; the executor reads only through them.
The actual moka-backed `CachedReader` with commit-seq version stamping (§3–§4)
is deliberately **not** built yet — arch gates moka-vs-hand-rolled on traversal
benchmarks (§7 open-Q 1), and there is nothing to size against until the
engine has a realistic workload. The seam means adding it later touches no
executor code. Cacheable reads already return `Arc`s so the trait signature
won't change when the cache arrives.

Scope: an in-memory, read-through cache sitting between the storage layer and
the computation layer. The executor never talks to storage directly; it talks
to the cache, which resolves misses from the storage snapshot.

## 1. What this layer caches — and what it deliberately doesn't

redb (and any mmap/file-backed KV) already benefits from the **OS page
cache**, so caching raw pages or raw KV values here would mostly duplicate
memory. The unique win at this position is caching **decoded, graph-shaped
objects**, saving the B-tree walk *and* the codec decode on hot paths:

| Cached | Keyed by | Payload |
|---|---|---|
| Node records | `node_id` | labels + decoded `Map<String, PropDesc>` |
| Edge records | `edge_id` | src/dst/type + decoded properties |
| Adjacency segments | `(node_id, dir, edge_type?)` | compact `Vec<(neighbor_id, edge_id)>` |
| Dictionaries | — | label/edge-type/prop-key interning tables (small, cached whole) |
| Catalog snapshot | — | latest soft-schema aggregate |

Not cached in v1: query results, plans, vector search results (the HNSW index
is already an in-memory structure), and raw KV pages (OS's job).

Traversal is the motivating workload: a 2-hop expansion touches the same hub
nodes' adjacency segments and records over and over; decoding a `PropDesc`
map per visit is pure waste.

Planes need no special handling here: node/edge IDs are globally unique
across planes, so ID-keyed entries can't collide, and a record's plane is
part of its cached payload. `drop_plane` invalidates through the same
commit-sequence mechanism as any other write (§3).

## 2. Interface

The computation layer sees a `GraphReader` trait; the cache is its primary
implementation, wrapping a storage read transaction:

```rust
trait GraphReader {
    fn node(&self, id: NodeId) -> Result<Option<Arc<NodeRecord>>>;
    fn edge(&self, id: EdgeId) -> Result<Option<Arc<EdgeRecord>>>;
    fn neighbors(&self, id: NodeId, dir: Dir, ty: Option<TypeId>)
        -> Result<Arc<AdjSegment>>;
    fn dict(&self) -> &Dictionaries;
    fn catalog(&self) -> Arc<CatalogSnapshot>;
}

struct CachedReader<'a> { cache: &'a GraphCache, txn: StorageReadTxn<'a>, snapshot: CommitSeq }
struct UncachedReader<'a> { txn: StorageReadTxn<'a> }   // pass-through, for tests/benchmarks
```

Entries are `Arc`-shared and immutable — a cache hit is a clone of an `Arc`,
never a copy of the record. Immutability is safe because MVCC records never
mutate in place; a write creates a new version.

## 3. Consistency with MVCC

The cache must never serve a record version the reader's snapshot shouldn't
see. Design: **version-stamped entries** over the backend's commit sequence.

- Every committed write transaction gets a monotonically increasing
  `CommitSeq` (maintained in `meta`, mirrored in memory).
- A cache entry records the range `[created_at, invalidated_at)` of commit
  seqs for which it is valid. A reader at snapshot `S` hits only if
  `created_at ≤ S < invalidated_at`.
- **Single-writer makes this cheap**: on commit, the writer knows exactly
  which node IDs / edge IDs / adjacency keys it touched. Commit closes those
  entries (`invalidated_at = new_seq`) and may insert fresh versions
  (write-through for records it already holds decoded).
- Old versions are evicted normally; a long-running reader on an old snapshot
  that misses simply falls through to storage — correctness never depends on
  the cache retaining anything.
- Crash safety: the cache is memory-only and rebuilt cold on open. It holds
  no durable state, so it can never corrupt the database.

## 4. Eviction and sizing

- Policy: **W-TinyLFU** via the `moka` crate (sync API) as the v1 default —
  scan-resistant, which matters because `ScanLabel` over a big label must not
  flush the hot traversal working set. A hand-rolled sharded LRU is the
  fallback if `moka`'s overhead shows up in benchmarks.
- Budget: single configurable byte budget for the whole layer (default:
  modest, e.g. 64 MiB — embedded-library ethos; hosts can raise it). Adjacency
  segments and node records share the budget; entries are weighted by
  approximate heap size.
- Oversized entries (a hub node with a 10M-edge adjacency segment) bypass the
  cache above a per-entry cap rather than evicting everything else; the
  executor streams them from storage.

## 5. Observability

`db.stats()` exposes per-table hit/miss/eviction counters and current byte
usage. Benchmarks at M2/M5 gate the layer: if the cache doesn't clearly win on
traversal-heavy workloads vs `UncachedReader`, its default budget shrinks —
the trait boundary keeps it removable.

## 6. Testing strategy

- Differential: every computation-layer test runs against both `CachedReader`
  and `UncachedReader`; results must be identical.
- Snapshot-isolation tests: interleave writer commits with readers pinned to
  old snapshots; assert no future-version leaks (the `[from, until)` check).
- Eviction-under-pressure fuzz: tiny budget + random workload, assert
  correctness (falls through) and bounded memory.

## 7. Open questions

1. ~~**`moka` vs hand-rolled** — measure overhead per hit at M2.~~ **Resolved:
   moka.** Its overhead never showed up against the decode cost it saves, so
   the concurrency machinery was not the overkill it looked like. The
   `CachedReader` micro-benchmark that gated this decision lives on in
   `core/benches/graph.rs`; the native engine uses the same crate for its SST
   block cache.
2. **Write-through vs invalidate-only on commit** — write-through warms the
   cache with the writer's decoded records but risks polluting it with bulk
   ingest; possibly ingest-mode toggles to invalidate-only.
3. **Adjacency segment granularity** — whole neighbor-list-per-(node, type) vs
   fixed-size chunks for hub nodes. Chunking interacts with the per-entry cap.
4. **Negative caching** — cache "node X does not exist"? Useful for
   external-key lookups during ingest dedup; decide with real ingest traces.
5. **Catalog/dictionary coherence** — both are tiny and versioned; confirm
   they can just be `ArcSwap`-published on commit rather than living in the
   main cache.
