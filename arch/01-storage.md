# Storage Layer

**Status**: draft for review · 2026-07-22

Scope: durable representation of the graph — KV backend abstraction, key
encodings, property serialization, identifiers, vector index persistence,
transactions. Everything above sees graph concepts (planes, nodes, edges,
properties, vectors), never raw KV keys.

The graph is partitioned into **planes** (exclusive namespaces — see
[09-planes.md](09-planes.md)); `plane_id` leads most keys so per-plane scans,
traversals, and drops are contiguous prefix ranges.

## 1. Backend: `StorageEngine` trait

The graph layer is written against a narrow trait so the backend is swappable
(and so a custom engine can replace it in v2 without touching upper layers):

```rust
trait StorageEngine {
    type ReadTxn<'a>: ReadTransaction where Self: 'a;
    type WriteTxn<'a>: WriteTransaction where Self: 'a;

    fn begin_read(&self) -> Result<Self::ReadTxn<'_>>;
    fn begin_write(&self) -> Result<Self::WriteTxn<'_>>;   // single-writer OK in v1
}

trait ReadTransaction {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Value>>;
    fn range(&self, table: TableId, range: impl RangeBounds<[u8]>)
        -> Result<impl Iterator<Item = Result<(Key, Value)>>>;
}
// WriteTransaction: ReadTransaction + put/delete/commit/abort.
```

**v1 backend: [`redb`](https://github.com/cberner/redb)** — pure Rust,
single-file, ACID with MVCC (concurrent readers + one writer), no C++ build
chain, a good fit for the embedded-first shape.

RocksDB remains a candidate second backend if write-heavy ingest becomes the
bottleneck; the trait exists precisely so that is a contained change. An
in-memory backend (BTreeMap) is also implemented early — it powers fast tests
and property-based testing against a model.

## 2. Identifiers

- Internal node and edge IDs are monotonically allocated `u64`s, **globally
  unique across all planes**, never reused. Global allocation means
  copy/move-between-planes can never collide and a bare ID is unambiguous in
  logs and APIs. Allocation counters persist in the `meta` table, bumped in
  batches to avoid a write per allocation.
- Plane IDs are `u32`s allocated from `meta`; `plane_id = 0` is the default
  plane (`"main"`). Plane names are unique, stored in the `planes` table.
- An optional `external_key → node_id` table (per plane) supports
  user-supplied stable keys (e.g. entity URIs from an extraction pipeline);
  external keys are also the identity thread for cross-plane entity
  resolution.
- Label names and edge-type names are interned to `u32`s in a dictionary
  table shared across planes; all keys store interned IDs, not strings.
  Dictionaries are small and cached in memory.

## 3. Logical tables and key layout

All integers big-endian so byte-order sorting gives the scan order we need.
`·` denotes concatenation of fixed-width fields.

| Table | Key | Value |
|---|---|---|
| `meta` | well-known keys | format version + magic, ID counters, dictionaries, codec version |
| `planes` | `plane_id` | name + property blob (`Map<String, PropDesc>`) |
| `plane_names` | `name` | `plane_id` |
| `nodes` | `plane_id · node_id` | label-id set + property blob |
| `edges` | `plane_id · edge_id` | `src · dst · type_id` + property blob |
| `adj_fwd` | `plane_id · src_id · type_id · dst_id · edge_id` | ∅ |
| `adj_rev` | `plane_id · dst_id · type_id · src_id · edge_id` | ∅ |
| `label_idx` | `plane_id · label_id · node_id` | ∅ |
| `ext_keys` | `plane_id · external_key` | `node_id` |
| `prop_idx` (opt-in) | `plane_id · prop_id · encoded_value · node_id` | ∅ |
| `node_plane` | `node_id` | `plane_id` (reverse lookup: resolve a bare ID) |

Notes:

- **Traversal is a prefix range scan** on `adj_fwd`/`adj_rev`: all
  out-neighbors of a node — optionally restricted to one edge type — are one
  contiguous scan within the node's plane. This is the classic KV-graph
  encoding (Dgraph/JanusGraph family), and is the piece a custom v2 engine
  would replace with a CSR-like layout.
- **Cross-plane edges are rejected here**: `create_edge` verifies src and dst
  resolve in the same plane and errors otherwise (see
  [09-planes.md](09-planes.md) §1).
- **Plane drop is a prefix range-delete** on each plane-prefixed table plus
  removal of the plane's vector-index sidecars — no per-record garbage
  collection.
- **Parallel edges are allowed**: `edge_id` is part of the adjacency key, so
  multiple same-type edges between the same pair coexist. Extraction
  pipelines produce these routinely.
- **Deletes**: deleting a node removes its adjacency entries (both
  directions), its `label_idx`/`ext_keys`/`prop_idx`/`node_plane` entries, and
  all incident edges, in one write transaction. No tombstones at this layer —
  the KV handles space reclamation.

## 4. Properties: soft schema, self-describing

A property set is an open map — no fixed columns, no DDL; any node/edge may
gain or lose properties at any time. Each property carries not just a value
but an optional natural-language description, making records
**self-describing** — an LLM (via MCP) can read *and write* what a property
means, not only what it holds:

```
Properties = Map<String, PropDesc>

PropDesc {
    description: Option<String>,   // human/LLM-readable meaning of this property
    value:       PropValue,
}

PropValue = Null | Bool | Int(i64) | Float(f64) | Str(String)
          | Bytes(Vec<u8>) | Vector(Vec<f32>) | List(Vec<PropValue>)
          | Map(BTreeMap<String, PropDesc>)
```

Notes:

- `description` is `None` for the common case and costs one byte in the
  encoding when absent; property maps without descriptions pay essentially
  nothing.
- Nested `Map` values reuse `PropDesc`, so descriptions are available at every
  nesting level.
- Descriptions are **data, not schema**: two nodes may describe the same
  property key differently. The soft-schema catalog (computation layer)
  aggregates per-`(label, property)` descriptions and surfaces the dominant
  ones through introspection — that aggregate is what the MCP layer serves to
  LLMs as "schema", descriptive rather than prescriptive.
- Codec: MessagePack vs `postcard` vs custom — decided by a small benchmark on
  decode-heavy traversal workloads (M1). Codec version recorded in `meta`.
  Reads must be able to skip descriptions cheaply (traversal filters touch
  values, not descriptions), which the benchmark should measure.
- **Enforcement lives elsewhere**: storage never rejects a shape. Advisory
  constraints (warn/reject modes) can come later in the catalog without any
  storage change.
- Large values (long text, big vectors): stored inline in v1. If blobs start
  dominating page churn, add an overflow table keyed by `(record_id, prop_id)`
  — tracked as an open question.

## 5. Vector storage and index

- `Vector` properties are stored in the record blob (**the KV is the single
  source of truth**) and additionally registered in a vector index when one is
  declared for a `(plane, label, property)` triple — each plane's index is its
  own sidecar, so plane drop is a file delete and small planes can skip
  indexing entirely (brute force below a threshold).
- `VectorIndex` trait mirroring `StorageEngine`'s role:

  ```rust
  trait VectorIndex {
      fn insert(&mut self, id: u64, vec: &[f32]) -> Result<()>;
      fn remove(&mut self, id: u64) -> Result<()>;
      fn search(&self, query: &[f32], k: usize, filter: Option<&IdFilter>)
          -> Result<Vec<(u64, f32)>>;
      fn persist(&self, path: &Path) -> Result<()>;
  }
  ```

- v1 implementation: HNSW (`usearch` / `hnsw_rs` / hand-rolled — benchmark at
  M3 before committing), persisted as a **sidecar file** next to the DB file.
- Consistency: index updates ride on the write-transaction commit; the sidecar
  carries the committed transaction ID it reflects. On open, version mismatch
  or corruption ⇒ rebuild from the KV. WAL-integrated durability is a v2
  concern.
- `search` takes an optional ID filter so the executor can push label/property
  predicates into the ANN search (filtered HNSW) instead of over-fetching.

## 6. Transactions

Inherited from redb in v1:

- Serializable MVCC — many concurrent readers, one writer.
- Readers get a stable snapshot; long traversals never block writes and are
  never torn by them.
- Write batching at the graph layer (e.g. bulk ingest API) amortizes commit
  cost; the single-writer ceiling is an accepted v1 constraint (revisit if
  ingest throughput demands a RocksDB backend or group-commit machinery).

## 7. Testing strategy

- Property-based tests: random operation sequences applied both to the real
  engine and to a naive in-memory model graph; states must match.
- Crash-recovery: kill-during-commit harness over redb's guarantees, plus
  vector-sidecar staleness recovery.
- The in-memory `StorageEngine` backend keeps the full upper-layer test suite
  fast.

## 8. Open questions

1. **Property codec** — MessagePack vs postcard vs custom (benchmark, M1);
   must support cheap skipping of `description` fields on the hot read path.
2. **HNSW library vs hand-rolled** — recall + build/query speed benchmark (M3).
3. **Overflow storage for large blobs** — needed in v1 or defer?
4. **Adjacency value payload** — should `adj_fwd` duplicate a few hot edge
   properties (e.g. weight) to avoid an `edges` lookup during weighted
   traversal? Measure first.
5. **Single-writer ceiling** — acceptable through v1? Revisit at M5 benchmarks.
6. **Description indexing** — should property descriptions themselves be
   embeddable/searchable (e.g. semantic search over what properties mean)?
   Natural M3+ extension; no storage change needed if they stay in the blob.
7. **`node_plane` reverse table vs encoding plane into the node ID** — packing
   `plane_id` into the high bits of `node_id` would kill the extra lookup but
   caps planes/IDs and complicates copy/move (ID changes on move). Current
   choice: separate table; revisit if the resolve hop shows up in profiles.
8. **Vector-index sidecar proliferation** — thousands of tiny planes ⇒
   thousands of files? Likely fine because small planes don't build indexes;
   confirm, or pack sidecars into one directory-per-db container file.
