# API Layer (DB core surface)

**Status**: shipped · living design notes, begun 2026-07-22

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

Opening is not a write. `Database::open` (and `open_read_only`) commits
only what a read shows to be missing — the initial meta of a fresh database,
a format migration, a counters row for a plane that predates them — and
otherwise leaves the commit sequence exactly where the last writer put it.
A replica (`serve --follow`) depends on this: its sequence is the master's,
landed by `apply_replicated`, and a bootstrap commit per open would run it
ahead so that the next replicated batch moved it backwards.

`Database` root carries only plane lifecycle (`create_plane`, `drop_plane`,
`planes()`), cross-plane operations (`copy`, `move_`, stack reads), global
catalog roll-up, and `stats()`. Everything else hangs off a `PlaneHandle`.
`drop_plane` removes everything the plane owns, including the `meta` rows
that sit outside its key prefix — the summary counters and the vector and
keyword index declarations — and the matching live registry entries, so a
dropped plane's indexes are neither listed nor rebuilt on the next open.
Plane ids are never reused.

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
- Index management: `paper.ensure_vector_index("Person", "embedding",
  Metric::Cosine)?` and `paper.ensure_keyword_index("Doc", "body",
  Language::English)?` — declarative, idempotent, per plane. Only the
  declaration is durable (a `meta` row); the index itself is rebuilt from the
  KV on open unless a sidecar stamped with the current commit sequence is
  found (01 §5).
- `commit()` returns `Ok` exactly when the KV commit is durable. Everything
  it does afterwards — mirroring buffered events into the in-memory vector
  and keyword registries, building the change-feed set — is applied in full,
  one failure never skipping the rest, and can only log: an `Err` here would
  make the caller take a committed write for a failed one. A vector-index
  event that fails marks the registry *diverged*: it keeps serving (still
  the better answer than none) but is never persisted at the current
  sequence — `save_sidecars` withholds the `.hnsw` sidecar and removes the
  stale one, `snapshot` embeds a fresh rebuild — so the next open rebuilds
  from the KV, which is always the truth.
- The change feed (`Database::on_change`) is bounded at the source: a write
  txn tracks at most 256 distinct entities; a mutation of a further entity
  is not buffered, only noted, and the delivered `ChangeSet` is flagged
  `truncated` (possibly with an empty list) so a subscriber knows that seq
  changed more than it can see. Entities within the cap still collapse
  (create-then-delete cancels). A bulk load of a million nodes buffers 256
  tuples, not a million.
- `restore` always leaves the live vector and keyword registries matching
  the restored data: the snapshot's index frames are loaded when they parse
  at the restored seq (and persisted to the sidecar paths where the database
  has any — an in-memory one has none), otherwise the registries are
  rebuilt from the restored KV.

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

Time travel: `plane.as_of(AsOf::Seq(s) | AsOf::Time(ms))?` returns a handle
whose every read is pinned to that snapshot (native backend only). The
vector and keyword registries are not versioned — they describe the latest
commit — so index-backed terminals are answered from the snapshot instead:
vector searches brute-force the pinned records and keyword searches compute
BM25 over them (both exact, both unindexed: one label scan per query). A
historical keyword or hybrid result is the snapshot's own — the nodes that
matched at the pinned point, on the text they held then, scored against that
corpus — so a node deleted, relabelled or re-texted afterwards is still found
and one created or made to match afterwards is not. The uncached reader takes
the same exact path on every read, which is what makes it the oracle for the
indexed one. Live reads have a narrower hazard: a writer publishes its KV
commit before it updates the registries, so an index can briefly name a node
the snapshot cannot decode; the reader drops such ids rather than surfacing a
phantom row (02 §3).

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

1. ~~**`props!` / `Expr` ergonomics** — macro vs builder-only.~~ **Resolved:
   builder-only.** No `props!` macro was ever written. Properties are built
   from plain iterators of `(String, PropDesc)` and `Expr` from the free
   functions (`p`, `lit`, `has_label`, `score`, …), which read well enough at
   call sites that a macro would only have hidden the types.
2. **Streaming rows vs `collect()` defaults** — iterators are right for the
   core, but wrappers keep collecting; provide `rows()` + `all()` both?
3. ~~**Blocking writer acquire** — timeout default? Fail-fast for MCP
   callers?~~ **Resolved: bounded on request, unbounded by default.**
   `Database::set_write_timeout` bounds the wait for the single writer slot and
   raises `Error::Timeout` when it expires. The default stays unbounded because
   for an embedded caller — the only writer — waiting *is* the correct
   behaviour, and failing a write that would have succeeded a moment later
   would be a regression. A process serving several clients sets a bound
   instead: there, one long `bulk_load` or `digest` otherwise blocks every
   other writer for its whole transaction with nothing to say why (08 §4.2).
   It takes `&self`, so a server can set it through the `Arc<Database>` it
   already holds. Still to do once `/mcp` lands: give the timeout its own
   JSON-RPC code, since `app()` currently flattens every core error onto one
   and a caller should be able to tell "retry this" from "your request was
   wrong".
4. ~~**Wire-protocol readiness** — decide the serialization format when the
   server wrapper lands.~~ **Resolved: JSON-RPC 2.0 over the wire, postcard on
   disk.** The two codecs are deliberately different: postcard's versioned
   wire-format spec is what durable data needs, while the network surface has
   to be readable by five SDKs and a browser. `crates/dr-strange-web/openrpc.json`
   is the source of truth for that surface — the SDKs are generated from it and
   drift-tested against it, and the server returns it verbatim from
   `rpc.discover`.
