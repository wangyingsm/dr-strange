# Web UI Layer

**Status**: v1 shipped (locked 2026-07-29), v2 drafted 2026-08-09 · begun
2026-07-22

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
  edge color by type, size by degree or score. Up to 400 nodes the layout
  runs synchronously (a fixed iteration count finishes before a worker could
  spawn); above that it runs in graphology's ForceAtlas2 web worker for a
  bounded wall-clock budget (2 ms a node, 0.5–4 s — `layoutPlan` in
  `frontend/src/layout.js`), so "show all" on a large plane converges in
  view instead of freezing the tab. Sectoring, packing and focus follow once
  the forces stop; a new layout, or leaving the page, kills a run in flight.
- **Hub-safe incremental expansion**: click-to-expand neighborhoods with
  bounded fan-out and "N more…" affordances — the UI never asks the core for
  an unbounded dump (cursors throughout). *Expand one hop* takes at most 300
  frontier nodes a click and asks for them in JSON-RPC batches (`rpcBatch`
  in `frontend/src/rpc.js`: `MAX_BATCH` a request, one request in flight),
  so a click is a handful of round trips rather than one socket per node.
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
- The server's own linear scans are paged and stop early, since they run
  on a header keystroke. `plane.find` walks the plane `SCAN_PAGE` (2 000)
  node records at a time and stops at `limit` hits or `FIND_SCAN_CAP`
  (20 000) nodes examined; its edge pass stops at the same cap of nodes
  visited or edges examined, so a needle matching no edge on a plane of
  leaves costs the cap, not a neighbour lookup per node; `graph.seed
  order=degree` measures the degree of at most `SEED_SCAN_CAP` (20 000)
  nodes into a bounded top-`limit` heap; both take `total` from the
  transactional counters (03 §5), so no request ever holds every node
  *record* of the plane at once. What a page still costs is the core's id
  scan: the executor collects the plane's node ids (8 bytes each) before
  its skip/limit steps apply, so each page is O(plane) in ids and a walk to
  the cap is at most `cap / SCAN_PAGE` such scans — a resumable id cursor
  in the core scan (03) is the open item that would remove it. The core
  keeps no per-node degree, so a degree is one neighbour lookup — the cap
  is what bounds a degree seed.
- Nothing in the core may assume a TTY or block indefinitely without a
  cancellation path.

## 4. Security model

**Status**: v1 shipped (locked 2026-07-29) · v2 drafted 2026-08-09 for the
multi-machine deployment ROADMAP §10 opens up.

### 4.1 v1 — one shared token (shipped)

Two independent layers, defending against *different* attackers:

- An **Origin guard** rejects a browser request whose `Origin` isn't loopback
  (or an exact entry in `DRSG_ALLOWED_ORIGINS`). This defeats cross-site
  (CSRF / DNS-rebinding) writes, which binding to localhost alone does **not**.
  `Origin` is browser-set and a page cannot forge it, which is what makes it a
  usable CSRF signal. Native clients send no `Origin` and sail past this layer —
  the *token*, not the Origin, is what authenticates them.
- A **bearer token** (`DRSG_TOKEN`) gates the whole surface, for every client.

`Access::{Read, Write, Admin}` is named explicitly at every dispatch arm, so a
new method cannot ship ungated by omission. Under one shared token all three
tiers require the same secret; the distinction exists so scoped keys can
separate them later. `Authorizer` is a deliberate seam for exactly that.

**Zero-config fallback.** With no token set, only the same-origin browser UI is
trusted and every programmatic client is denied *even for reads* — so a desktop
install doesn't quietly expose an open API on localhost.

This model is sound while `drsg serve` is a loopback tool. It does not survive
contact with §4.2, and the fallback becomes actively dangerous there — so the
server enforces where the line is (shipped 2026-09, `server::run`,
`assets.rs`, `auth.rs`):

1. **A non-loopback bind requires a token.** `run()` refuses to start on
   any address that is not loopback unless `DRSG_TOKEN` is configured, and
   the error names the variable and the `--addr` way back. Without a token
   the only credential left is an `Origin` header any client can type.
2. **The fallback is gated on a loopback bind, a loopback origin and an
   unforwarded request.** `SharedToken` knows whether its listener is
   loopback-bound, and `resolve_credentials` sets `local_ui` only for an
   `Origin` that is itself loopback, on such a listener, on a request
   carrying no proxy header; an allowed `Origin` on a LAN bind, or a
   configured public origin on any bind, still passes the CSRF guard but
   authorizes nothing by itself. A public origin in `DRSG_ALLOWED_ORIGINS`
   is the operator saying a proxy serves the dashboard to the network, so
   `check_bind_policy` also refuses a tokenless server with one configured,
   whatever the bind address. Invariant 2 of §4.2, closed.
3. **The page carries the token only to a local human.** `GET /` is
   unauthenticated (it must be — it is how the browser gets the page that
   will authenticate), so whatever is in the page is public to whoever can
   fetch it. The token is spliced into `index.html` only when every sign
   says a browser on this machine asked for it (`assets::may_inject_token`):
   the bind *and* the peer address are both loopback, the request carries
   none of `Forwarded` / `X-Forwarded-*` / `X-Real-IP` / `Via`, its `Host`
   is this machine, no public origin is configured, and the operator has
   not set `DRSG_PAGE_TOKEN=0`. A same-host reverse proxy or a forwarded
   port shows a loopback peer for every client on the internet, which is
   why a loopback peer alone is not enough and why the switch exists for a
   proxy that adds nothing the listener can see. On any other deployment
   the page is served bare, and the SPA asks for the token the first time
   the server answers unauthorized, keeping it in the tab's
   `sessionStorage`.
4. **The token is data, not code.** It rides a `<meta name="drsg-token">`
   element, never an inline `<script>`, and every response carries a
   `Content-Security-Policy` of `script-src 'self'` (and `style-src 'self'` —
   Svelte and sigma style through the CSSOM, which CSP does not govern) with
   `worker-src blob:` for the layout worker and `img-src data:` for the
   inlined logo. An injected script cannot run, whatever else goes wrong.
5. **The WebSocket takes a header first.** `/ws` accepts `Authorization:
   Bearer` on the upgrade — the form every non-browser client should use,
   since a header reaches neither access logs nor browser history — and
   `?token=` only because the browser WebSocket API cannot set headers. The
   header wins when both are present. Nothing in the server logs a request
   target, so a query-string token never reaches a log line.
6. **A wrong token is guessed five times, then waited for.** A per-peer
   throttle (`auth::FailedAuthLimiter`, applied as a middleware over the
   whole router so no route can forget it) counts bearers that authorize
   nothing; past `FREE_FAILURES` (5) the peer serves a wait that doubles per
   failure up to `MAX_LOCKOUT` (5 min), answered `429` with `Retry-After`
   before the request is read further. A correct bearer clears it. A request
   with no bearer is not a guess and is neither counted nor blocked, so the
   zero-config local UI is untouched. The table is in memory and bounded
   (`TRACKED_PEERS`, 4096; idle entries are forgotten after 15 min) — a
   client rotating addresses weakens the throttle for itself, not the
   server. A JSON-RPC batch is at most `rpc::MAX_BATCH` (64) requests,
   refused whole with `-32600` above that.
7. **A provider named over the wire is a preset or the operator's.** Every
   method that takes a `provider` / `embed_provider` / `chat` / `embed`
   field, and `POST /cypher?embed=`, resolves it through one helper
   (`methods::provider_for`): the name is one of the llm crate's presets
   (`is_preset`) or exactly the provider the operator configured
   (`[server] embed_provider`), and anything else — a base URL — is `-32602`.
   The llm crate accepts a URL because an operator at a terminal means one;
   the server would be posting to it from its own network on behalf of
   whoever holds a read credential, which is a request forgery. The
   dashboard offers presets only.
8. **A cost knob has a ceiling the request cannot move.** `plane.ask`'s
   `max_attempts` defaults to and is capped at the llm crate's
   `ASK_DEFAULT_ATTEMPTS` (20), its `limit` at `ASK_MAX_LIMIT` (1000);
   `digest.run` with `apply: true` is **not** confirm-gated the way the MCP
   `digest` tool is (arch/06 §3), by decision rather than omission: a
   JSON-RPC caller is an authenticated program presenting a write token, and
   `apply` is the explicit choice it made — a second flag would be the same
   choice spelled twice. The MCP gate exists because there the caller is a
   model, and the cost of an unintended apply (rewriting a plane) is what
   the confirmation buys back. The dashboard's own digest page keeps the
   review-then-apply flow in its UI.
   `digest.run`'s `concurrency` and `chunk_chars` are clamped to
   `DIGEST_MAX_CONCURRENCY` (32) / `DIGEST_MAX_CHUNK_CHARS` (32 000) or the
   operator's own `[digest]` default, whichever is larger. `/export`
   streams (`server::stream_body`: a blocking producer writing 64 KiB
   chunks into a bounded channel) rather than build the whole body in
   memory, and its walk holds the plane's node ids (one scan, 8 bytes each)
   and one record at a time, never every record; the status line is chosen
   after the request is validated, so a bad plane is still a `400`, and a
   failure mid-stream truncates the chunked body, which is the one honest
   signal left. `/snapshot` is different: `Database::snapshot` holds the
   registry read locks and a read transaction for as long as it writes, so
   streaming it straight to the client would hold every commit on the
   master for the follower's whole download. It is spooled to an anonymous
   temp file under those locks (`server::spool_snapshot`) and the file is
   streamed by the runtime's file reader — the locks are held for one local
   write of the image, and a slow follower pins neither a lock nor a
   blocking-pool thread.
9. **An error says only what the client may know.** Core `Io` / `Backend` /
   `Corrupt` errors (the database path, a backend's internals), provider
   *call* errors (the upstream reply body) and plugin-store errors (the
   store directory) go to the log at `warn` under a short reference, and
   the client gets the category and the reference — `storage error (ref
   00002a)` — through `methods::opaque`. Client faults keep their text:
   unknown plane, bad plan, a provider with no key or embedding model in
   the environment (decided before any network call, from strings this
   process composed).
10. **`/mcp` answers at loopback, and at the names the operator lists.**
    The MCP transport's DNS-rebinding guard checks the `Host` header
    against a list `server::mcp_allowed_hosts` builds: `localhost`,
    `127.0.0.1`, `::1` always; with a bearer token configured, the bind
    address (when it names one — a wildcard does not) and every entry of
    `ServeOptions::allowed_hosts` / `DRSG_ALLOWED_HOSTS`. Without a token
    the extras are ignored and logged, because the guard is then doing the
    work the Origin guard does for browsers: a tokenless server trusts its
    same-origin UI, and a rebinding page impersonating it is what a
    loopback-only `Host` defeats. The list is never empty — rmcp reads an
    empty list as "any host".
11. **Background work is bounded and single-flight.** A stale plugin
    catalog starts one refresh (`methods::refresh_catalog_once`, a flag
    plus the runtime's blocking pool), however many panels ask while it
    runs. A follower's replication queue holds `follow::REPLICATION_QUEUE`
    (1024) batches; when it is full the socket reader waits, the master's
    broadcast lags the follower out, and the design's answer — a full
    resync — happens visibly instead of after memory runs out. The fetch
    guard judges an IPv6 address carrying an IPv4 one (v4-mapped, 6to4,
    Teredo, NAT64 `64:ff9b::/96`) as that IPv4 address, and refuses the
    local-use NAT64 block `64:ff9b:1::/48` whole.

### 4.2 v2 — many agents, many machines, one database

**Driver.** Three requirements: LAN-reachable UI and RPC so ops can maintain the
database; agents on *several machines* sharing one database; and ops light
enough that a team will actually run it.

**The shape is forced, not chosen.** The native engine holds an exclusive
advisory lock on `<dir>/LOCK` for its lifetime (01; shipped v1.4.2), so exactly
one process may open a database directly. Agents on other machines therefore
*cannot* embed it — they must be network clients of one `drsg serve`. A second
server binary sharing the same directory is not an option.

That leaves one process with **two listeners**:

| Listener | Audience | Surface | Authenticates by |
|---|---|---|---|
| `--addr` | ops humans | dashboard + `/rpc` + `/ws` | session (browser) or token |
| `--mcp-addr` | agent hosts | `/mcp` | per-agent token |

Separate addresses and ports let the two differ in network exposure — the ops UI
on loopback or a management VLAN while `/mcp` faces the LAN — and the MCP
listener carries no browser surface at all, so CSRF and XSS do not apply to it.

**Authentication differs per listener; authorization does not.** Both listeners
reach the same `Arc<Database>` with the same powers, so splitting the credential
mechanism splits neither the blast radius nor the audit trail. Two front-ends,
one core.

**Credentials: scoped tokens.** `drsg_<keyid>_<secret>` — `keyid` gives O(1)
lookup and a safe prefix to show in the UI and the audit log.

- Store `SHA-256(secret)`, never the secret. A 256-bit random token is
  high-entropy, so it needs no password stretching (argon2 is slow because
  *passwords* are weak); a fast hash suffices, and a database read-compromise
  yields nothing usable.
- Revocation is a row delete — what you need when an agent machine is lost.

Three alternatives were considered and rejected:

- *Challenge-response* (`hash(secret + nonce)`) forces the server to store
  something password-equivalent in order to verify it, trading protection in
  transit — which TLS already provides — for plaintext at rest. It also requires
  the browser to hold the raw secret in JS, where XSS can take it.
- *Per-agent Ed25519 request signing* is stronger, but costs key distribution,
  rotation, clock-skew handling and a nonce cache, and no off-the-shelf MCP
  client speaks a bespoke signing scheme. Behind TLS on a trusted LAN the
  marginal gain does not pay for the ops burden. Revisit if `/mcp` ever faces
  the internet — and adopt RFC 9421 rather than inventing a canonicalization.
- *OAuth 2.1* buys MCP-spec interop but needs an authorization server. Too much
  infrastructure for the deployment this targets.

**Authorization: one core, scoped by plane.** `Authorizer` returns a `Principal`
rather than a `bool`, so the audit log records *which* agent made a change — for
shared knowledge, attribution is worth more than stronger crypto. A principal
carries a plane scope and an `Access` level, and planes (09) are the isolation
unit: a team's agents get `Write` on their own plane, `Read` on a shared one,
nothing elsewhere. This answers §10's isolation fork without inventing a new
concept.

**Invariants for any non-loopback bind.** Each is a footgun today:

1. **TLS is required.** Bearer tokens over plaintext are readable by anyone on
   the segment.
2. **The zero-config `local_ui` fallback must not apply.** It grants full write
   access when no token is configured, and it keys off *allowed origin*, not
   *loopback*. Once an operator adds the LAN UI to `DRSG_ALLOWED_ORIGINS` — which
   requirement 1 forces — an unset `DRSG_TOKEN` means any LAN browser has
   unauthenticated write access. *Shipped* (§4.1): the fallback is gated on a
   loopback bind, and a tokenless server refuses to bind anywhere else.
3. **`DRSG_ALLOWED_ORIGINS` must name the ops UI origin**, or the dashboard
   cannot call its own backend. The page the LAN UI loads carries no token
   (§4.1); the browser asks for it and sends it as a bearer credential.

**Accepted limits.** One process owns the database, so it is a single point of
failure and a throughput ceiling for *writes* — `write_gate` serializes them,
one at a time, no matter how many clients ask. Read-heavy knowledge sharing
with occasional writes fits comfortably on that one process; scaling reads out
further no longer needs a proxy tier — `serve --follow` (arch/01 §9) runs a
read-only replica of a running `drsg serve`, so a cluster of agents can spread
reads across several processes without funnelling them all through one. The
lock still makes *write* scale-out (several processes sharing one writer) a
one-way door — that remains unbuilt, and `serve --follow` doesn't touch it: a
replica is read-only for its whole lifetime, never promoted.

## 5. Decisions since drafting

The questions this doc opened with in July are answered by shipped code;
recorded here so the rationale isn't lost.

1. **Rendering stack** — Svelte + Vite (bun) for the app; graphology as the data
   model with sigma's WebGL renderer for plots (§2.2).
2. **Layout for large neighbourhoods** — client-side ForceAtlas2
   (`graphology-layout-forceatlas2`). No core-assisted layout hints were needed.
3. **Live updates** — push, not polling. Every commit broadcasts its `ChangeSet`,
   and each `/ws` subscriber that ran `plane.watch` drains its own receiver and
   receives `plane.change`. A subscriber that falls too far behind loses the
   overflow rather than stalling the writer (ROADMAP §5).
4. **Does `drsg serve` fold into the network server?** — yes; it *is* the network
   server. §4.2 and ROADMAP §10 settle it.
5. **Read scale-out** — `serve --follow` (arch/01 §9), a read-only replica
   layered on the same `Authorizer` seam this doc designed: a `ReadOnlyAuthorizer`
   decorator that never grants `Write`/`Admin`, a third gate alongside the
   Origin guard and the bearer token, needing no per-method change to any of
   the RPC methods this doc's `Access` levels already tag.

Still open:

5. **Dashboard history charts** (ingest rate, plane growth over time) have no
   storage. The core stays stateless by design, so this needs either a small
   ring buffer in `drsg serve` or client-side sampling. Unbuilt.
