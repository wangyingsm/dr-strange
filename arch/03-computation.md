# Computation Layer

**Status**: draft · query engine + native hybrid search + catalog built
(M2–M3) · 2026-07-28

**M2 landed** the non-hybrid core: `Source`
(ScanAll/ScanLabel/SeekIds/SeekKeys) + `Step`
(Expand/ExpandVar/Filter/Skip/Limit/Distinct/Sort), a serializable
`LogicalPlan`, a total `Expr` evaluator, and a pull-based executor over
`GraphReader` (`compute::{plan, expr, exec}`). The **row model is the linear
pipeline** of §2 (a current node + trail + an optional `f32` score channel),
not the named-multi-variable form. `Project` is a v0 API terminal
(`select`), not a plan `Step`.

**M3 landed** the AI-native surface: the hybrid operators of §4 —
`Source::VectorTopK`, `Step::FrontierTopK`, `Step::ExpandBeam` — executed
natively (one plan, one snapshot; never API aggregation), with the score
channel and `score()`/`hops()`/`distance()`/`similarity()` fusion in `Expr`.
Vector search is exact by default and index-accelerated when a `(plane,
label, property)` index is declared (`crate::index`, arch/01 §5). The
soft-schema **catalog** of §5 is built (`compute::catalog`, `plane.catalog()`
/ `db.catalog()`), computed by full scan; incremental maintenance is the
remaining optimization.

Design commitment up front: **hybrid search is an executor capability, not
API-level aggregation.** A query mixing traversal and similarity is ONE plan
running over ONE snapshot, with operators that interleave graph and vector
access. The caller never runs a graph query and a vector query separately and
joins the results — that pattern is slower (over-fetching, two passes) and
wrong (two snapshots can disagree).

## 1. Plane context

Every plan executes against a **plane context** ([09-planes.md](09-planes.md)):

- v1: exactly one plane per query — the partition model's natural unit. The
  plane is fixed at plan build time (from the `PlaneHandle`), so operators
  compile straight to plane-prefixed scans; there is no per-row plane check.
- v1.5: **stack reads** — the same plan fanned out across a set of planes,
  results concatenated and tagged with their plane. Planes share nothing, so
  this is embarrassingly parallel; cross-plane top-k (vector or scored) is a
  k-way merge since scores share one metric.

## 2. Logical plan

A small algebra of operators. The v1 builder API constructs these plans
directly; the v2 query language parses into the **same** plans — this seam is
what makes "QL later" cheap.

Core operators (v1):

| Operator | Meaning |
|---|---|
| `ScanLabel(label)` | all nodes with a label (via `label_idx`) |
| `SeekId(ids)` / `SeekExternalKey(keys)` | direct lookups |
| `Expand(dir, edge_type?)` | 1-hop neighbors via adjacency |
| `ExpandVar(dir, edge_type?, min..max)` | bounded variable-length expansion |
| `Filter(expr)` | predicate on node/edge property values |
| `Project(exprs)` | select/rename outputs, incl. property, description, and score access |
| `Limit(n)` / `Sort(keys)` / `Distinct` | usual suspects |

Hybrid operators (v1 — semantics in §4):

| Operator | Meaning |
|---|---|
| `VectorTopK(label, prop, qvec, k)` | plane-wide ANN search — a *seed* source |
| `FrontierTopK(binding, prop, qvec, k)` | top-k by similarity **within an existing binding** — graph-constrained vector search |
| `ExpandBeam(dir, type?, prop, qvec, width, depth)` | similarity-**guided** traversal: expand, score neighbors against `qvec`, keep best `width` per step |
| `Score(expr)` | attach/replace a score on a binding (distance, fusion, structural terms) |

Plans operate over *bindings* (named node/edge variables) that carry an
optional **score channel**: an `f32` per row that hybrid operators produce
and `Filter`/`Sort`/`Score` consume like any other value. An `Expand`
consumes a binding and produces a new one alongside it, so multi-hop patterns
keep every intermediate variable addressable.

`Filter`/`Project`/`Score` use a small serializable `Expr` enum (property
access, literals, comparisons, boolean/arithmetic ops, `distance()`,
`score()`, `description()`, fusion helpers — §4.5) rather than Rust closures:
closures are un-serializable and would block the v2 query language and any
wire protocol. A `FilterFn` escape hatch taking a closure exists for embedded
users, documented as unavailable over the wire.

## 3. Execution

- **Pull-based iterator model** over a stable snapshot: one read transaction
  + one `GraphReader` per query. Simple, streaming, cancellable; `Arc`-shared
  cache entries make repeated visits to hub nodes cheap.
- Batched (morsel-style) execution only where profiling shows it pays —
  likely `Expand` over hot adjacency segments and the vectorizable distance
  loops in hybrid operators. Start scalar; batch later.
- **No cost-based optimizer in v1.** Only obvious rewrites:
  - predicate pushdown into scans;
  - use `prop_idx` when a filtered property is indexed;
  - direction flip on `Expand` when the reverse adjacency is cheaper;
  - pushdown of filters into ANN search (§4.6).
- Parallelism: queries run on the caller's thread in v1 (embedded-library
  ethos). Intra-query parallelism arrives with stack reads (v1.5) where the
  per-plane independence makes it trivial.

## 4. Hybrid graph + vector search (native)

Four patterns, all single plans. The first two are the composition everyone
supports; the last two are what "AI-native" buys and are why the dedicated
operators exist.

### 4.1 Seed-then-expand
`VectorTopK → Expand → Filter → Project` — "find entities similar to the
query, then examine their neighborhoods." Hits carry their similarity in the
score channel, which survives expansion (each downstream row remembers its
seed's score) for later fusion.

### 4.2 Traverse-then-rerank
`ScanLabel → Expand → Score(distance(n.embedding, qvec)) → Sort → Limit` —
"collect candidates structurally, re-rank semantically." Computes distances
against stored vectors directly; no index involved.

### 4.3 Graph-constrained vector search — `FrontierTopK`
"Among the papers *cited by anything in this cluster*, which are most similar
to the query?" The candidate set is a **binding produced by traversal**, not
a whole label. The executor picks a strategy per call, adaptively:

- **small frontier** (below a threshold, e.g. ≲ 4k rows): brute-force exact
  distances over the frontier's stored vectors — cheap, exact, no index
  needed;
- **large frontier**: filtered ANN — the frontier's ID set becomes the
  `IdFilter` pushed into `VectorIndex::search` (the storage layer's HNSW
  supports filtered search precisely for this);
- the crossover threshold is a tunable, benchmarked at M3.

This operator is what makes API aggregation unnecessary: without it, the
caller would pull the whole frontier out, run a separate vector query, and
intersect — two passes, two snapshots, no ranking guarantee.

### 4.4 Similarity-guided traversal — `ExpandBeam`
Beam search through the graph, steered by embedding space: at each step,
expand the current frontier, score every neighbor against `qvec`, keep the
best `width`, repeat to `depth`. Rows carry cumulative path and score. This
is the GraphRAG exploration primitive — "walk toward the query's meaning" —
and cannot be expressed as post-hoc aggregation of separate searches.

### 4.5 Score fusion
Hybrid results need one rank from several signals. The `Expr` vocabulary
includes fusion helpers usable in `Score`/`Sort`/`Filter`:

- `distance(a, b)` / `similarity(a, b)` — cosine / dot / L2;
- `score()` — the binding's current score channel (e.g. seed similarity);
- weighted linear fusion — plain arithmetic over the above plus structural
  terms (`hops()`, `degree()`, edge properties such as weights);
- `rrf(rank_a, rank_b, k)` — reciprocal-rank fusion, for combining two
  orderings without score calibration.

Example — one plan, no aggregation: "top 10 Documents within 2 hops of
vector-seeded Entities, ranked by `0.7·seed_similarity + 0.3·1/hops`" =
`VectorTopK → ExpandVar(1..2) → Filter(label = Document) →
Score(0.7*score() + 0.3/hops()) → Sort → Limit(10)`.

### 4.6 Filter pushdown into ANN
When a plan filters on label or an indexed property *and* vector-searches the
same binding, the rewrite pushes the predicate into the ANN call as an
`IdFilter` (label bitmap / index-scan result) instead of over-fetching
top-k′ ≫ k and post-filtering. Over-fetch-with-retry remains the fallback for
non-indexed predicates.

## 5. Soft-schema catalog

The catalog is the *descriptive* view of the data that makes "no DDL" usable.
It is **per plane** (each canvas has its own shape) with a cheap roll-up
across planes:

- Maintained incrementally on every write (and rebuildable by full scan):
  which labels exist; which property keys appear per label; observed value
  types and frequencies; edge-type connectivity (which label pairs each edge
  type actually links); which `(label, property)` pairs have vector indexes.
- **Aggregates `PropDesc.description`s** per `(label, property)`: dominant
  descriptions become the property's documented meaning in introspection
  output; conflicting descriptions are surfaced, not silently resolved.
- Served through the API as `plane.catalog()` (and `db.catalog()` roll-up).
  This is exactly what the MCP layer presents to LLMs as "schema" —
  descriptive, never prescriptive.
- Persistence: catalog tables live in the KV, updated in the same write
  transaction as the data they describe; the latest snapshot is published to
  readers via the cache layer.
- Later (post-v1): advisory constraints — warn or reject on shape drift — as
  a catalog feature, with no storage-layer change.

## 6. Graph algorithms (post-v1 direction)

BFS/shortest-path arrive early via `ExpandVar`. Whole-graph analytics
(PageRank, communities, connected components) are a separate module that
materializes a working set from a snapshot; out of v1 scope, noted so
plan/executor decisions don't preclude it.

## 7. Testing strategy

- Golden-plan tests: builder input → expected plan (catches rewrite
  regressions).
- Executor conformance: same query against redb-backed and in-memory
  backends, cached and uncached readers — four-way identical results.
- Hybrid correctness: `FrontierTopK`'s brute-force path is exact, so it is
  the oracle for the filtered-ANN path (recall bounds, not exact equality);
  `ExpandBeam` tested against exhaustive expansion + rerank on small graphs.

## 8. Open questions

1. **`Expr` surface** — the M2 set (property access, literals, `HasLabel`,
   `IsNull`, comparisons with numeric coercion, boolean logic, arithmetic;
   total evaluation — incomparable/missing ⇒ predicate false) shipped and is
   enough for filter/sort. **String operators and membership have since
   landed**: `StringMatch` (`CONTAINS` / `STARTS WITH` / `ENDS WITH`, byte-wise
   like `=`, with scalars promoted to their text form so soft-schema data
   whose type varies per node still matches) and `In` (`List` by element under
   `=` equality, `Map` by key). Membership is deliberately a separate operator
   rather than an overload of `CONTAINS`: for a list of strings, "contains"
   reads as either element-equality or existential substring, nothing in the
   syntax picks one, and openCypher splits them the same way — so a query
   written against it means the same thing here. Still deferred:
   **edge-property access**, which needs the richer binding model.
2. **`ExpandVar`/`ExpandBeam` path semantics** — **v0 chose walk semantics**
   for `ExpandVar`: bounded by depth `min..=max`, nodes/edges may repeat, one
   row per distinct walk; callers add `Distinct` for uniqueness. Trail /
   simple-path modes and the `ExpandBeam` revisit-vs-dedup question are
   revisited when the richer binding model + `ExpandBeam` land (M3).
3. ~~**Score channel: one or many?**~~ **Resolved (M3): a single channel.**
   Real fusion queries fold their signals through `Expr` arithmetic over
   `score()`, `hops()` and `distance()` rather than needing named columns, so
   the simple operators won. Hybrid retrieval (ROADMAP §2) shipped on this
   model.
4. **`FrontierTopK` crossover threshold** — brute force vs filtered ANN;
   benchmark at M3.
5. **Catalog write amplification** — per-write incremental updates vs
   periodic fold; measure at M3.
6. **Timeout/cancellation** — cooperative check per iterator `next()`;
   confirm granularity is enough for MCP request deadlines.
