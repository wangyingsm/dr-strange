# Web UI Layer

**Status**: draft for review · 2026-07-22

Scope: a local-first UI with two jobs in v1 — a **dashboard** over the
database and **visual graph plots** for exploration. Build starts post-M4
(the core must exist first), but dashboard + visualization are v1 features,
not nice-to-haves.

## 1. Shape

- A thin local server (`drsg serve`, in `dr-strange-cli` or a small `dr-strange-web` crate)
  embedding `dr-strange-core`, serving a bundled single-page app — same
  embedded-first ethos, no separate backend deployment.
- The backend API is **JSON-RPC 2.0** (project-wide wire protocol,
  00-overview §2) over HTTP POST, with a WebSocket upgrade for streaming
  results and live updates. Methods map 1:1 to the public core API
  (`plane.query`, `plane.catalog`, `db.stats`, …); serialized plans/values
  ride as params verbatim — the same structures MCP uses, so this backend is
  the first draft of the eventual network server, not a bespoke one-off.

## 2. v1 features

### 2.1 Dashboard

Landing view: the state of the database at a glance.

- **Plane overview**: the pile of canvases as cards/table — name,
  description, node/edge counts, vector-index coverage, last-write time;
  create/drop (gated) from here.
- **Database health**: file size, cache hit rates, transaction counters,
  sidecar index freshness (`db.stats()` rendered live over the WebSocket,
  `CommitSeq` as the change token).
- **Per-plane catalog panel**: labels, property keys with dominant
  `PropDesc` descriptions, observed types/frequencies, edge-type
  connectivity matrix — the soft schema made visible.
- **Activity**: recent digest/import runs with provenance (source, model,
  run id) once `dr-strange-llm` provenance lands.

### 2.2 Graph plots (visual exploration)

- **Interactive plot canvas**: force-directed layout, WebGL-rendered so
  thousands of visible nodes stay smooth; pan/zoom, node color by label,
  edge color by type, size by degree or score.
- **Hub-safe incremental expansion**: click-to-expand neighborhoods with
  bounded fan-out and "N more…" affordances — the UI never asks the core for
  an unbounded dump (cursors throughout).
- **Hybrid search overlay**: search box → embedding (via `dr-strange-llm` if
  configured) → `VectorTopK`/`FrontierTopK`; hits highlighted on the plot
  with similarity scores; `ExpandBeam` walks animate the traversal path.
- **Plane switcher** on the plot: one plane at a time in v1 (partition
  model); stacked side-by-side comparison of two planes is the v1.5 follow-up
  to stack reads.
- **Record inspector**: selecting a node/edge shows properties **with
  descriptions** (self-describing data pays off visually).
- Read-only by default; editing behind an explicit toggle.

## 3. Constraints on other layers (why this doc exists now)

- Core results must stay streamable/pageable (cursors) — incremental
  expansion depends on it (03/04 already provide this).
- Catalog and stats must be serializable structs (04 §4) — they become
  JSON-RPC results verbatim; stats granular enough to drive the dashboard.
- The executor's score channel must be surfaced in rows (done — 03 §2), so
  plots can size/color by score without recomputation.
- Nothing in the core may assume a TTY or block indefinitely without a
  cancellation path.

## 4. Open questions

1. Rendering stack — WebGL graph library (e.g. sigma.js/cosmos-style) vs
   hand-rolled; frontend framework choice. Decide when work starts.
2. Layout for large neighborhoods — client-side force simulation vs
   core-assisted layout hints (degree, clusters) for >10k-node plots.
3. Live updates — JSON-RPC notifications over the WebSocket, keyed by the
   cache layer's `CommitSeq` as the change token?
4. Does `drsg serve` fold into the future network server, or stay a separate
   local-only tool?
5. Dashboard charts (ingest rate, plane growth over time) need history —
   does the core persist counters over time, or does the UI sample and store
   client-side? (Leaning: core stays stateless; `drsg serve` keeps a small
   ring buffer.)
