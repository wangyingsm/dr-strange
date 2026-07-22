# Planes

**Status**: draft for review · 2026-07-22

A **plane** is a canvas of graph: the database is a pile of planes, each
holding its own nodes, edges, and vectors. Planes are the unit of scoping,
isolation, and lifecycle — "one plane per document, per extraction run, per
agent session, per hypothesis" is the intended usage pattern for AI
workloads.

This is not a horizontal layer in the stack; it is a **fourth primitive of
the data model** (Plane, alongside Node/Edge/Vector) that threads vertically
through storage, cache, computation, and API. This doc defines the model;
the layer docs carry the mechanics.

## 1. Model: exclusive partition

- Every node and every edge lives in **exactly one plane**. Planes are hard
  namespaces — separate rooms, not transparent overlays.
- The same real-world entity appearing in two planes is two distinct nodes;
  connecting them is an entity-resolution act (shared `external_key`
  convention, LLM-layer helpers — see [07-llm.md](07-llm.md)), never implicit.
- **Cross-plane edges are rejected** at the storage layer in v1. If a bridge
  concept proves necessary, it arrives later as an explicit `portals` table —
  tracked as an open question, deliberately not smuggled into the edge model.
- Planes are lightweight: creating one writes one record. Hundreds or
  thousands of planes (one per ingested document) is a supported shape, not
  an abuse.

## 2. Planes are self-describing records

A plane has an identity and its own open property map, same as nodes/edges:

```
Plane {
    plane_id:   u32,                      // interned, key-prefix friendly
    name:       String,                   // unique, human/LLM-facing
    properties: Map<String, PropDesc>,    // description, provenance, created_by,
                                          // source URI, agent session id, ...
}
```

`PropDesc` (see [01-storage.md](01-storage.md) §4) applies here too — a plane
can carry a natural-language description of what this canvas *is*, which the
MCP layer surfaces so an LLM can choose where to read and write.

- A **default plane** (`plane_id = 0`, name `"startup"`) exists from creation, so
  single-canvas users never think about planes at all.
- Plane names are unique and interned; all keys carry the `u32` id, never the
  name.

## 3. Lifecycle and operations

| Operation | Semantics | Cost shape |
|---|---|---|
| `create_plane(name, props)` | new empty canvas | O(1) |
| `drop_plane(id)` | delete the canvas and everything on it | prefix range-delete per table + drop of its vector index files |
| `rename / set properties` | metadata update | O(1) |
| `copy(selection, from, to)` | copy a subgraph into another plane: **new node/edge IDs**, `external_key`s carried along (they are the cross-plane identity thread) | O(selection) |
| `move(selection, from, to)` | copy + delete; edges fully inside the selection move, edges crossing the selection boundary are dropped (reported to the caller) | O(selection) |
| `merge(from…, to)` | v2, sits on `copy` + entity resolution (LLM layer proposes matches by external key / embedding similarity; core executes the copy plan) | — |

Each operation is one write transaction (or a batched sequence for huge
planes — same bulk-ingest machinery as import).

## 4. How planes thread through each layer

- **Storage** ([01-storage.md](01-storage.md)): `plane_id` is the leading
  component of node, adjacency, label-index, and external-key table keys, so
  every per-plane operation — scan, traversal, drop — is a contiguous prefix
  range. Node/edge IDs remain globally unique `u64`s (allocation is global),
  so copies/moves never collide and IDs stay unambiguous in logs and APIs.
- **Cache** ([02-cache.md](02-cache.md)): unchanged keys — records and
  adjacency segments are keyed by globally unique IDs; a node's plane is part
  of its cached record. Plane drop invalidates by plane, which the
  version-stamp mechanism already covers.
- **Computation** ([03-computation.md](03-computation.md)): every plan
  carries a **plane context**. v1: exactly one plane per query (the partition
  model's natural unit). A *stack read* — the same plan run across several
  planes with results concatenated and tagged by plane — is a cheap v1.5
  addition (planes share nothing, so it is embarrassingly parallel), useful
  for "search across all document-planes."
- **Vector indexes**: declared per `(plane, label, property)`; each plane's
  index is its own sidecar. Small planes fall below the brute-force threshold
  and never build one. Plane drop = delete sidecar files. Cross-plane
  similarity search (v1.5, with stack reads) = fan-out + k-way merge of
  per-plane top-k — trivially correct because scores share one metric.
- **API** ([04-api.md](04-api.md)): a `PlaneHandle` is the scoping object —
  `db.plane("startup")` returns one; all reads/writes hang off it. `db` root
  keeps only plane lifecycle + cross-plane ops (`copy`, `move`, stack reads).
- **Catalog / MCP**: the soft-schema catalog is **per plane** (each canvas
  has its own shape), with a cheap roll-up across planes. MCP tools take a
  plane argument and expose plane listing/descriptions so an LLM can navigate
  the pile.

## 5. Why partition (decision record)

Chosen over *overlay* (shared nodes, plane-scoped edges) and *soft tags*
(filter-only views):

- Simplest possible encoding — one prefix, no membership tables, no
  visibility rules in every operator.
- Real isolation: a runaway agent session cannot corrupt `main`; review-then-
  merge becomes the natural workflow.
- Cheap, safe lifecycle: dropping an extraction run is a range delete, not a
  garbage-collection problem.
- The cost — duplicated entities across planes — is accepted and addressed
  explicitly by external keys + entity resolution, which an AI-native system
  needs anyway (extraction pipelines produce duplicates even inside one
  plane).

## 6. Open questions

1. **Portals / bridge edges** — if "this node in plane A corresponds to that
   node in plane B" needs to be queryable in-core (not just via shared
   external keys), add an explicit cross-plane correspondence table. Defer
   until a real workload demands traversing it.
2. **Stack-read surface** — v1.5: is per-plane result tagging enough, or do
   callers need cross-plane dedup by external key built in?
3. **Plane quotas/limits** — max planes, max per-plane size warnings for the
   embedded profile?
4. **Copy-on-write plane forking** — `fork_plane` for cheap "try a hypothesis
   on a copy of main" would be powerful but fights the prefix encoding
   (shared immutable base + delta). v2+ investigation, likely tied to the
   custom storage engine.
