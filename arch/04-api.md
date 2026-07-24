# API Layer (DB core surface)

**Status**: draft for review · 2026-07-22

Scope: the public Rust API of `dr-strange-core` — the only surface wrappers (CLI,
MCP, LLM helpers, future server) are allowed to use. Design rule: **every
capability of the engine is reachable from this API**, so wrappers never need
private hooks; and everything here maps to serializable plans/values, so a
wire protocol later is mechanical.

## 1. Shape

```rust
let db = Database::open("knowledge.dr")?;          // or Database::in_memory()

// Planes are the scoping object (09-planes.md)
let startup = db.plane("startup")?;                     // default plane, always exists
let paper = db.create_plane("paper-2406.01234", props! {
    "source" => desc("arxiv URL this plane was extracted from", "https://arxiv.org/abs/2406.01234"),
})?;
```

`Database` root carries only plane lifecycle (`create_plane`, `drop_plane`,
`planes()`), cross-plane operations (`copy`, `move_`, stack reads), global
catalog roll-up, and `stats()`. Everything else hangs off a `PlaneHandle`.

## 2. Writes

```rust
let mut txn = paper.write()?;                      // one write txn (single writer)

let alice = txn.create_node(
    &["Person", "Author"],
    props! {
        "name"      => "Alice",
        "embedding" => desc("text-embedding-3-small of the bio", vec_f32(emb)),
    },
)?;
let post = txn.create_node_with_key("arxiv:2406.01234", &["Paper"], props! { ... })?;
let authored = txn.create_edge(alice, post, "AUTHORED", props! { "position" => 1 })?;

txn.set_prop(alice, "affiliation", desc("current employer, from §1 footnote", "MIT"))?;
txn.remove_prop(alice, "draft_flag")?;             // soft schema: shrink freely
txn.set_edge_prop(authored, "verified", desc("checked against ORCID", true))?;

txn.delete_edge(authored)?;   // idempotent: deleting twice is not an error
txn.delete_node(alice)?;      // cascades to alice's remaining incident edges
txn.commit()?;
```

- `props!` builds `Map<String, PropDesc>`; plain values get
  `description: None`, `desc(text, value)` attaches one. Descriptions are
  data — writable and readable like any value.
- `create_node_with_key` errors with `Conflict` if the key is already bound
  to a different node in this plane (arch/01 §2); `delete_node`/`delete_edge`
  are idempotent — deleting an absent record is `Ok(())`, matching the
  storage layer's posture (arch/01 §3).
- Bulk ingest: `txn.bulk()` returns a batching writer (amortized commits,
  cache in invalidate-only mode) used by CLI import and MCP ingest tools.
  Node/edge id allocation is already batched under the hood (arch/01 §2) —
  `bulk()` mainly needs to batch *commits*, not ids.
- Vector index management: `paper.ensure_vector_index("Person", "embedding",
  Metric::Cosine)?` — declarative, idempotent, per plane.

## 3. Reads and queries

Handles are cheap and `Send`; reads run on a stable snapshot; results stream
as iterators rather than materializing.

```rust
// Direct access
let n = paper.node(alice)?;                        // Arc<NodeRecord>
let neighbors = paper.neighbors(alice, Dir::Out, Some("AUTHORED"))?;

// Query builder → logical plan → executor (03-computation.md)
let results = paper.query()
    .vector_top_k("Paper", "embedding", &qvec, 25)          // seed
    .expand_out("CITES")                                    // graph step
    .filter(p("year").ge(2020))                             // Expr, serializable
    .score(fuse(0.7 * score() + 0.3 / hops()))              // fusion
    .sort_desc(score())
    .limit(10)
    .rows()?;                                               // streaming iterator

// Graph-constrained vector search — one plan, no client-side joins
let related = paper.query()
    .seek_key("arxiv:2406.01234")
    .expand_var(Dir::Both, None, 1..=2)
    .frontier_top_k("embedding", &qvec, 20)
    .rows()?;
```

Builder methods mirror plan operators one-to-one (`ScanLabel`, `Expand*`,
`VectorTopK`, `FrontierTopK`, `ExpandBeam`, `Filter`, `Score`, …); the
builder is a thin, type-checked plan constructor, not a DSL with its own
semantics. The v2 query language compiles to the same plans.

Row values: `Row` exposes bound variables by name → `NodeRef`/`EdgeRef` with
`id()`, `labels()`, `prop(key)`, `prop_desc(key)` (value + description),
`score()`.

## 4. Introspection

```rust
let cat = paper.catalog()?;         // per-plane soft schema (labels, props,
                                    // observed types/frequencies, dominant
                                    // descriptions, vector indexes, counts)
let all = db.catalog()?;            // roll-up across planes
let st  = db.stats()?;              // cache hit rates, sizes, txn counters
```

Catalog output is a plain serializable struct — the MCP layer renders it for
LLMs nearly verbatim.

## 5. Cross-plane operations

```rust
let sel = Selection::nodes(&ids).with_induced_edges();
db.copy(&sel, &paper, &startup)?;      // new IDs in `main`; external keys carried
db.move_(&sel, &paper, &startup)?;     // copy + delete; boundary-crossing edges reported

// Stack read (v1.5): same plan across many planes, rows tagged by plane
let hits = db.stack(db.planes_matching("paper-*")?)
    .query().vector_top_k("Chunk", "embedding", &qvec, 10).rows()?;
```

## 6. Errors, threading, async

- One `dr_strange_core::Error` enum with typed variants (NotFound, PlaneMismatch —
  e.g. cross-plane edge attempts, Conflict, Io, Corrupt, …); wrappers map
  variants to exit codes / MCP error payloads. (Open question in overview:
  confirm single-enum at M0.)
- Sync core. `Database` is `Send + Sync`; write transactions serialize on the
  single writer (blocking acquire with timeout). Async wrappers put blocking
  calls on their runtime's blocking pool — the core does not depend on tokio.
- Cancellation: `query().with_deadline(d)` / `.with_cancel(token)` —
  cooperative checks in the executor (03 §8.6).

## 7. Stability policy

Pre-1.0: additive evolution preferred, breaking changes batched into marked
releases; the on-disk format version (`meta`) is independent of API version.
`unsafe` is forbidden in `dr-strange-core` except vetted dependencies.

## 8. Open questions

1. **`props!` / `Expr` ergonomics** — macro vs builder-only; how `desc()`
   reads at call sites. Prototype at M2 and dogfood via the CLI.
2. **Streaming rows vs `collect()` defaults** — iterators are right for the
   core, but wrappers keep collecting; provide `rows()` + `all()` both?
3. **Blocking writer acquire** — timeout default? Fail-fast for MCP callers?
4. **Wire-protocol readiness** — plans and values are serializable by
   construction; decide serialization format (likely the same codec as
   properties) when the server wrapper lands.
