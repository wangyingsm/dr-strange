# Web UI

`drsg serve` serves a single-page dashboard, embedded in the binary, for
inspecting and operating a database from a browser. It uses the same JSON-RPC API
as the SDKs over HTTP, and a WebSocket for live updates. Being embedded, it makes
no external network requests: all assets are served from the same origin, and all
rendering is local.

## Access and authentication

Open the address reported by `drsg serve` (default `http://127.0.0.1:7700`). When
a token is configured, the server injects it into the served page, so the
same-origin UI authenticates automatically; when no token is configured, the
same-origin origin check authorizes the local UI. Cross-origin requests are
refused regardless.

The interface has three views, selected from the header: **Dashboard**,
**Explore**, and **AIgest**.

## Dashboard

The Dashboard presents database health and plane management.

- **Health.** A grid of statistics — planes, nodes, edges, labels, edge types,
  declared indexes, average degree, commits (the commit sequence), and on-disk
  size. These are pushed live over the WebSocket, so they update as the database
  changes; a connection indicator shows the live/offline state.
- **Plane cards.** One card per plane, showing its name, description, and
  node/edge counts. A card selects the plane (the app-wide context), exports it
  as JSONL (a download the CLI's `import` reads back), or deletes it behind a
  type-to-confirm dialog. A "New plane" card creates one.

## Explore

Explore is an interactive graph canvas driven by a tabbed toolbar. Selecting a
node or edge opens an inspector showing its labels/type and properties (vectors
are collapsed behind a control rather than printed); double-clicking a node
expands its neighborhood; dragging from one node to another opens the
new-edge dialog with the endpoints prefilled. A legend maps colors to labels.

The toolbar tabs:

- **Filters / Operations** — seed the canvas from a label (or the whole plane).
- **GraphQL / Run** — a query box for the Cypher subset ([Chapter
  4](./query-language.md)); a keyword ghost-hint completes clause keywords as you
  type, accepted with Tab. Reads plot their result; writes mutate and reload.
- **Algorithms** — run PageRank, connected components, shortest path, or Louvain
  and overlay the result on the current graph (scores as node size/color,
  components/communities as color groups).
- **Hybrid** — fused vector + keyword + graph-proximity search, with the channels
  and label selected in the bar.
- **Ask** — a natural-language question; the generated plan is shown (with a copy
  control) and its connected result is plotted.
- **Time-travel** — a slider over commit history (see below).
- **Live** — the change feed (see below).

## Search

The header carries a quick search over the current plane, in two modes:

- **Text** — substring matching across keys, labels, and string properties.
- **Semantic** — embedding-similarity ranking (with an embedding-provider
  selector).

Selecting a result focuses that node or edge in Explore. The search also respects
the time-travel cursor: with a past commit pinned, it searches the graph *as it
was* at that commit.

## Time-travel

On a native-backend server, Explore's **Time-travel** tab probes the queryable
window (`plane.history`) and, when available, presents a slider over the commit
sequence with a **Live** control at the latest end. Dragging back re-plots the
graph as of that commit; the seed and node expansions read the historical
snapshot. A marker on the canvas indicates the pinned commit on every tab, and
returns the view to live when dismissed. The header search reflects the same
cursor. On a non-native backend the tab is absent.

## Live feed

Explore's **Live** tab opens a `plane.watch` subscription over the WebSocket and
streams commits as they land, newest first: each entry shows the operation
(created / updated / deleted, color-coded), the kind (node / edge), the key or
id, the labels, and the commit sequence. The stream can be paused and resumed,
and narrowed to one label; selecting a node change focuses it in the canvas.

## AIgest

The AIgest view ingests a document into the current plane (see [Chapter
3](./ai-native.md)). Upload or paste text — Markdown, plain text, PDF, or DOCX —
choose the chat and embedding providers, and **Preview**: the model runs once and
proposes the entities and relations it extracted. **Write to graph** commits the
previewed proposal with no further model call. Options include linking to
existing nodes (to avoid duplicates) and skipping embeddings.

## Design

The dashboard is theme-aware (it follows the viewer's light/dark preference) and
fully self-contained: because it is embedded and same-origin, it depends on no
external CDN, font, or service, and issues no requests beyond the API and
WebSocket of the server that served it.
