# Web UI

`drsg serve` ships a single-page dashboard (embedded in the binary) for
exploring and operating a database from the browser. It talks to the same
JSON-RPC API the SDKs use, plus a WebSocket for live updates.

## The three views

- **Dashboard** — health at a glance (planes, nodes, edges, labels, edge types,
  indexes, average degree, commits, on-disk size, all live), plus plane cards to
  create, export, and delete planes.
- **Explore** — an interactive graph canvas driven by a tabbed toolbar:
  **Filters** (seed by label), **GraphQL/Run** (the Cypher box, with keyword
  hints), **Algorithms** (overlay PageRank / components / shortest path /
  Louvain), **Hybrid** (fused search), **Ask** (natural-language query),
  **Time-travel** (a slider to view the graph as of a past commit), and **Live**
  (stream mutations as they land).
- **AIgest** — upload or paste a document and turn it into graph structure with
  an LLM, previewing entities and relations before writing them.

## Header search

A quick search box (substring or semantic) over the current plane, which also
respects the time-travel cursor — search the graph *as it was* at a past commit.

## Sections (draft)

- Serving the UI and authenticating (token vs. same-origin local UI)
- The Dashboard: stat boxes and plane cards
- Explore: the canvas, selection/inspection, and each toolbar tab
- Search: quick vs. semantic, and searching a historical snapshot
- Time-travel in the UI: the slider and the "as of" indicator
- The Live feed: watching changes and jumping to a node
- AIgest: the ingest flow end to end
- A note on the design (theme-aware, embedded, zero external calls)
