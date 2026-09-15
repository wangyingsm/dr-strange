# Web UI

`drsg serve` serves a single-page dashboard, embedded in the binary, for
inspecting and operating a database from a browser. It uses the same JSON-RPC API
as the SDKs over HTTP, and a WebSocket for live updates. Being embedded, it makes
no external network requests: all assets are served from the same origin, and all
rendering is local.

## Access and authentication

Open the address reported by `drsg serve` (default `http://127.0.0.1:7700`). On a
loopback address, the UI needs no setup: with no token configured, the
same-origin check authorizes it; with `DRSG_TOKEN` set, the server writes the
token into the page it serves to a loopback browser (as a `<meta>` element the
app reads — never as a script), and the UI presents it as a bearer credential.
Cross-origin requests are refused regardless.

Served on any other address (`--addr 0.0.0.0:7700`, the container image), the
server requires a token to start at all and never writes it into the page,
because that page goes to anyone who can reach the port. The first request the
server answers *unauthorized* opens a prompt; paste `DRSG_TOKEN` there. It is
kept in the tab's session storage (gone when the tab closes) and sent as
`Authorization: Bearer` on every call and as `?token=` on the WebSocket, the only
form a browser socket can carry. The dashboard's own origin must also be listed
in `allowed_origins` / `DRSG_ALLOWED_ORIGINS` there, since it is not loopback.

A loopback bind behind a reverse proxy on the same machine (or a forwarded
port) is a network deployment, not a local one: every client on the internet
reaches the server from a loopback peer address. The server treats it as
such wherever it can tell — it never writes the token into a page whose
request a proxy forwarded (`Forwarded`, `X-Forwarded-*`, `X-Real-IP`, `Via`)
or addressed to a name other than this machine (`Host`), and listing the
proxy's origin in `allowed_origins`, which the dashboard needs to work
behind one, turns injection off for every page and makes a token mandatory
at startup (an allowed origin off loopback never counts as the local UI
either). A proxy that adds no header and keeps a loopback `Host` cannot be
told from a local browser, so for that shape set `DRSG_PAGE_TOKEN=0`
(`[server] page_token = false`), which serves the page bare regardless; the
dashboard then asks for the token as it does on any other address. Set a
token whenever anything stands in front of the listener.

Every response carries a `Content-Security-Policy` under which scripts and styles
load only from the server itself; the dashboard is built to satisfy it, and a
reverse proxy in front should pass it through rather than replace it.

A wrong token may be presented five times; after that the server answers the
peer `429 Too Many Requests` with a `Retry-After` that doubles per further
failure, up to five minutes, until a correct token is presented. Requests with
no token are not counted, so a tokenless local dashboard never trips it. A
JSON-RPC batch holds at most 64 requests.

Wherever the interface asks for a provider — semantic search, natural-language
queries, AIgest — it offers the presets (`openai`, `deepseek`, `qwen`,
`ollama`), and the API accepts exactly those, or the one provider the server was
configured with (`[server] embed_provider`). A base URL is refused over the
wire, because the server would be calling it from its own network on behalf of
whoever holds a credential; an operator who wants a local or self-hosted
endpoint configures it on the server. Errors that would describe the server's
own disk or its provider's reply come back as a category and a reference
(`storage error (ref 00002a)`); the detail is in the server's log under that
reference.

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
new-edge dialog with the endpoints prefilled.

### Reading a dense graph

A plane of any size is more than a canvas can show at once, so Explore draws
the **skeleton** and lets you ask for the rest.

- **Ranked seeding.** The opening view is the 40 most connected nodes and the
  edges among them, not the first 40 the scan reaches — a legible shape rather
  than an arbitrary sample. *Show more* widens it to 100, 200, 500. Ranking is
  by degree; `graph.seed` also offers PageRank, but PageRank pools rank in
  sinks, so a hub can score below its own neighbours.
- **The legend is a filter.** Click a label to hide that category, click again
  to bring it back. Hidden, not faded: a category you switched off is not part
  of the question.
- **Crowded hubs fold.** When a node has more than twenty *leaves* — neighbours
  attached to it and to nothing else — they collapse into one bead labelled
  with the count. Click the bead to open it. Nothing is discarded, and a leaf
  you have selected is never folded away.
- **Fans are grouped.** Below that threshold, a hub's leaves are arranged
  around it in arcs by label, so a ring reads as regions rather than a smear.
- **Importance opens the layout.** Edges touching a well-connected node are
  laid out longer, so a busy neighbourhood has room to be read.
- **A large layout runs off the main thread.** Past 400 nodes — *Show all*
  on a real plane — the force layout runs in a web worker for a bounded few
  seconds while the page stays responsive; the picture converges in view and
  settles when the run ends.
- **Expand one hop is batched.** Growing the frontier asks for at most 300
  nodes a click, in JSON-RPC batches of 64, and says how many were left for
  a second click and how many calls failed.
- **Selection focuses.** The selection and its immediate neighbours stay at
  full strength, the next ring dims, and everything beyond recedes and drops
  its label. Selecting an **edge** focuses both of its endpoints, since an edge
  is a statement about two things.

Node size tracks how connected a node is (counting anything folded beneath
it), and the legend maps colors to labels.

The toolbar tabs:

- **Filters / Operations** — seed the canvas from a label (or the whole plane).
- **Algorithms** — run PageRank, connected components, shortest path, or Louvain
  and overlay the result on the current graph (scores as node size/color,
  components/communities as color groups).
- **Hybrid** — fused vector + keyword + graph-proximity search, with the channels
  and label selected in the bar.
- **Ask** — a natural-language question; the generated plan is shown (with a copy
  control) and its connected result is plotted.
- **Time-travel** — a slider over commit history (see below).
- **Live** — the change feed (see below).

## Query

A view of its own, beside Dashboard and Explore, and the one place a query is
written: Explore draws the graph, this view asks it questions and reads the
answers ([Chapter 4](./query-language.md)).

The editor is several lines, since a query worth a page is rarely one: **⌘/Ctrl
+ Enter** runs it, plain Enter is a newline, and Tab accepts a keyword
completion ghost-hint as you type. Example queries sit under the editor as
starting points, and the text survives a reload.

The result takes the shape the query asked for:

- a **projection** (`RETURN n.file, count(*) AS n`) comes back as a table, with
  its row and column counts, the time it took, and a **copy** control that puts
  the whole thing on the clipboard as tab-separated text — header included, so
  it pastes into a spreadsheet;
- a query returning **whole records** (`RETURN n`) lists them by key, labels and
  the properties that fit;
- a **write** reports its change counts.

A query that cannot compile shows the parser's own message, which names the
construct rather than a character position.

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

The slider spans the *retained* window, not every commit ever made:
`plane.history` starts at the floor `[server] retain_commits` keeps (20 when
the key is omitted; set `retain_commits = 0` for unbounded history), the
readout says how many commits
that is, and `db.stats` reports the setting as `retain_commits`. Raise it
before you need the depth — versions past the floor are reclaimed at
compaction and cannot be reached afterwards.

## Live feed

Explore's **Live** tab opens a `plane.watch` subscription over the WebSocket and
streams commits as they land, newest first: each entry shows the operation
(created / updated / deleted, color-coded), the kind (node / edge), the key or
id, the labels, and the commit sequence. The stream can be paused and resumed,
and narrowed to one label; selecting a node change focuses it in the canvas.

## AIgest

The AIgest view ingests a document into the current plane (see [Chapter
3](./ai-native.md)). Upload or paste text — Markdown and plain text, or any of
Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV and PDF, which are
converted to Markdown so the model sees headings, tables and lists rather than
loose characters —
choose the chat and embedding providers, and **Preview**: the model extracts the
entities and relations and proposes them. **Write to graph** commits the
previewed proposal with no further model call. Options include linking to
existing nodes (to avoid duplicates) and skipping embeddings.

A **URL** row sits beside the upload control: paste an address, optionally a
topic, and **Fetch**. What comes back is not text in the box but a *list* — the
page and the pages it links to, each with its relevance score, title and size,
ticked if it cleared the floor. Unticking one removes it from the document with
no further request, and a fold-out shows what was not kept and why. Nothing
becomes tokens until the list looks right ([Chapter
3](./ai-native.md#reading-from-a-url)).

**Mode** selects how thoroughly the extraction is cleaned up — `coarse`, `fine`
(the default), or `super` — and is remembered like the provider choices, since
it is a standing preference rather than a per-run decision. Choosing `super`
raises a notice directly under the control: it re-reads every entity against all
of its passages, at roughly 15× the input token usage. See [Chapter
3](./ai-native.md#extraction-precision) for what each mode buys.

## Design

The dashboard is theme-aware (it follows the viewer's light/dark preference) and
fully self-contained: because it is embedded and same-origin, it depends on no
external CDN, font, or service, and issues no requests beyond the API and
WebSocket of the server that served it.
