# MCP Service Layer

**Status**: draft for review · 2026-07-22

Scope: the `dr-strange-mcp` crate — an MCP server that **embeds `dr-strange-core` directly**
(stdio transport, zero-ops: the host process owns the database file). This is
a primary interface, not an afterthought: tool shapes, result formats, and
token budgets are designed for an LLM consumer. Contains no database logic.

MCP speaks **JSON-RPC 2.0**, which is also the project-wide wire protocol
(00-overview §2) — so the serialized plan/value/catalog structures that ride
in MCP tool params/results are byte-identical to those of the web UI backend
and the future network server. One serialization, three surfaces.

## 1. Tool surface (v1)

Exploration-first, mirroring how an agent actually works a graph — orient,
then narrow, then act:

| Tool | Purpose |
|---|---|
| `list_planes` | planes with names, descriptions, sizes — "which canvas?" |
| `describe_plane` | per-plane catalog: labels, properties with **dominant descriptions**, edge-type connectivity, vector indexes, counts |
| `get_node` / `get_edge` | one record, properties **with descriptions**, adjacency summary (per-type counts, not the edges themselves) |
| `search` | one hybrid entry point: vector/structural/text — compiles to a plan (`VectorTopK` / `FrontierTopK` / label+filter) |
| `traverse` | bounded expansion from seeds (direction, edge types, depth, limit) |
| `query` | full serialized-plan execution for sophisticated callers |
| `write_nodes` / `write_edges` | batched upserts by external key; `PropDesc` descriptions writable |
| `digest` | LLM-powered document → graph ingest, same engine as `drsg digest` (05 §3; detailed design deferred to [07-llm.md](07-llm.md)) |
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

1. **`search` tool scope** — one polymorphic tool (fewer tools, kinder to
   small tool budgets) vs split `vector_search`/`find_nodes` (simpler
   schemas)? Prototype both against real agent transcripts.
2. **Embedding generation on write** — should `write_nodes` auto-embed text
   properties via `dr-strange-llm`, or must callers supply vectors? (Leaning: opt-in
   auto-embed configured per plane, provider set at server start.)
3. **Multi-database serving** — one MCP server per DB file vs a `db`
   argument on every tool.
4. **Transport** — stdio first; streamable HTTP when the server wrapper
   story lands.
