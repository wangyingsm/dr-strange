# MCP Service Layer

**Status**: shipped — `drsg-mcp` stdio (M4), streamable HTTP on `drsg serve`
(ROADMAP §10), `digest` tool live, agent verbs landed (§11) ·
last revised 2026-08-18

**M4 landed** the `drsg-mcp` stdio server on the **official `rmcp` SDK**
(resolving arch's hand-rolled-vs-SDK question toward the SDK: spec-correct
handshake/framing). Tools: list_planes, describe_plane (catalog), get_node,
search (vector), traverse, query (serialized plan), write_nodes, write_edges,
create_plane, drop_plane (confirm-gated). The sync core runs on
`spawn_blocking` so scans don't stall the async runtime; core errors surface
as MCP *tool-level* errors (the caller sees the message). Tool I/O uses the
shared `dr-strange-core::json` dialect. **`digest` is deferred** (arch/07).

Scope: the `dr-strange-mcp` crate — an MCP server that **embeds `dr-strange-core` directly**
(stdio transport, zero-ops: the host process owns the database file). This is
a primary interface, not an afterthought: tool shapes, result formats, and
token budgets are designed for an LLM consumer. Contains no database logic.

MCP speaks **JSON-RPC 2.0**, which is also the project-wide wire protocol
(00-overview §2) — so the serialized plan/value/catalog structures that ride
in MCP tool params/results are byte-identical to those of the web UI backend
and the future network server. One serialization, three surfaces.

## 1. Tool surface (current)

Exploration-first, mirroring how an agent actually works a graph — orient,
then narrow, then act:

| Tool | Purpose |
|---|---|
| `context` · `describe` · `search` · `grep` · `trace` · `impact` · `fathom` · `snippet` | the eight agent verbs over a digested code plane — one round trip each, compact one-fact-per-line text, ambiguity returns candidates, call listings state their lower bound. `context` is the primary verb; `grep` and `snippet` read the watched source tree |
| `list_planes` | planes with names, descriptions, sizes — "which canvas?" |
| `describe_plane` | per-plane catalog: labels, properties with **dominant descriptions**, edge-type connectivity, vector indexes, counts |
| `get_node` | one record, properties **with descriptions**, adjacency summary (per-type counts, not the edges themselves) |
| `traverse` | bounded expansion from seeds (direction, edge types, depth, limit) |
| `query` / `cypher` | serialized-plan execution; the openCypher subset compiled to the same plans |
| `algo` / `hybrid` / `ask` | graph algorithms; fused vector+keyword+proximity retrieval; NL → read-only plan |
| `write_nodes` / `write_edges` | batched creates by external key; `PropDesc` descriptions writable |
| `digest` | LLM-powered document → graph ingest, same engine as `drsg digest` (05 §3, [07-llm.md](07-llm.md)) |
| `create_plane` / `drop_plane` | canvas lifecycle (drop gated — §3) |

## 2. Token frugality (design rules)

- **Summaries before details**: every listing returns counts + exemplars with
  a cursor, never an unbounded dump; `get_node` summarizes adjacency instead
  of inlining neighbors.
- **Compact projection defaults**: ids, labels, name-ish properties, scores;
  full property maps (and descriptions) only on request or drill-down.
- **Vectors are never serialized back** to the model — similarity comes back
  as scores; embeddings are referenced, not printed.
- **Stable cursors** for pagination over streaming query results (one
  snapshot per cursor lifetime).
- Result envelopes carry `truncated: true` markers plus the exact follow-up
  call that fetches more — no silent caps.

## 3. Safety

- `drop_plane` and bulk deletes require an explicit `confirm: true` argument
  and, when called without it, echo what would be destroyed.
- Optional read-only mode (`--read-only`) for exploration deployments.
- Per-request deadlines map to executor cancellation (03 §8.6) so a runaway
  traversal cannot hang the host.

## 4. Why the catalog matters here

`describe_plane` is the payoff of soft schema + `PropDesc`: the LLM gets a
*descriptive* schema — which labels exist, what properties mean (aggregated
descriptions), how edge types actually connect labels — without anyone ever
writing DDL. Agents can also *improve* the graph's self-documentation by
writing descriptions back; future sessions inherit them.

## 5. Open questions

1. ~~**`search` tool scope** — one polymorphic tool vs split
   `vector_search`/`find_nodes`?~~ **Resolved: split.** Each tool carries a
   schema an agent can read without branching, and the tool budget never
   became the binding constraint the polymorphic option was hedging against.
   (The names have since evolved — `search` today is the semantic agent verb,
   with `traverse`/`query`/`cypher` beside it — but the one-tool-one-schema
   principle held.)
2. ~~**Embedding generation on write** — should `write_nodes` auto-embed text
   properties via `dr-strange-llm`, or must callers supply vectors?~~
   **Resolved: opt-in auto-embed, configured per server.** The `[server]`
   embed settings (`embed_provider` / `embed_model` / `embed_key_env`) drive
   `write_nodes` and the `search` verb alike; callers may still supply
   vectors directly.
3. **Multi-database serving** — one MCP server per DB file vs a `db`
   argument on every tool.
4. ~~**Transport**~~ — **settled, both shipped.** stdio remains the right
   answer for a single agent: point a host at a path, nothing to run.
   Streamable HTTP landed as an endpoint on `drsg serve` rather than a client
   mode here (ROADMAP §10), because the tools must run in-process against the
   same `Database` to keep batch atomicity. Its security model is 08 §4.2.
