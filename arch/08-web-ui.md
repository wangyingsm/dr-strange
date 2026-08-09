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
contact with §4.2, and the fallback becomes actively dangerous there.

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
   unauthenticated write access. Gate the fallback on a loopback bind.
3. **`DRSG_ALLOWED_ORIGINS` must name the ops UI origin**, or the dashboard
   cannot call its own backend.

**Accepted limits.** One process owns the database, so it is a single point of
failure and a throughput ceiling for every client. Read-heavy knowledge sharing
with occasional writes fits comfortably — `write_gate` serializes writers, MVCC
keeps readers concurrent. Scaling out later needs replication or a proxy tier;
the lock makes that a one-way door.

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

Still open:

5. **Dashboard history charts** (ingest rate, plane growth over time) have no
   storage. The core stays stateless by design, so this needs either a small
   ring buffer in `drsg serve` or client-side sampling. Unbuilt.
