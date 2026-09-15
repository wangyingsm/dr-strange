# MCP Service Layer

**Status**: shipped — `drsg-mcp` stdio (M4), streamable HTTP on `drsg serve`
(ROADMAP §10), `digest` tool live, agent verbs landed (§11), stdio relays to a
declared server when one is running ·
last revised 2026-09-14

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

- **What destroys is confirmed; what adds is not.** `drop_plane`, a `cypher`
  statement carrying `DELETE` or `REMOVE`, and `digest` with `apply: true`
  all require `confirm: true` and refuse without it, naming the flag. The
  gate is an MCP-surface rule, not a core one: the same digest over JSON-RPC
  (`digest.run`, arch/08) has no confirm flag, because that caller is an
  authenticated program holding a write token that chose `apply` itself,
  where an MCP tool is invoked by a model whose "apply" may be a guess. The
  cypher gate is a keyword scan of the statement (literals, backtick names
  and `.property` excluded) — the compiled statement's ops are the parser's
  own — and errs toward asking. Additive writes (`write_nodes`,
  `write_edges`, `CREATE`/`MERGE`/`SET`) are deliberately ungated: they are
  undoable by a later delete and are an agent's everyday annotation work,
  and a flag demanded for everything becomes a flag passed reflexively,
  which guards nothing. The tool descriptions carry the rule so a client
  knows when to pass the flag.
- **Files are read only from a tree, and only inside it.** Every read
  `grep` and `snippet` make passes one containment check — the path is
  refused by shape (absolute, `..`, a prefix) and then by where it
  canonicalizes to, which is what catches a symlink planted in a checkout.
  The file is then opened at the canonical path the check returned, never
  back through the link; what remains is the window between canonicalize
  and open in which a path component could be swapped for a link, which
  needs a writer racing the agent inside the tree and is accepted.
  Which trees exist is `TreeAccess`: the host-attached tree (`serve watch
  --dir`, `[server] source_root`) is always readable; a plane's own
  `synced_root` is data, honoured wherever it points only where the process
  already runs as the user (stdio, the CLI — `local_files`), and on the
  shared `/mcp` only when it lies inside the attached tree. Nothing attached
  and no local files: nothing is read. `digest { path }` remains stdio-only.
- **Every tool call ends.** The tool gate queues rather than rejects, but
  under one per-call deadline covering the wait and the run
  (`with_tool_deadline`; default 300 s; `DRSG_MCP_TOOL_DEADLINE_SECS`, `0`
  disables; `[server] mcp_tool_deadline_secs` says the same from the file,
  and as with every file key an environment variable already set wins over
  it). Past it the call is a *tool-level* error saying whether the
  server was busy or the body was slow. A running body is not cut short —
  blocking work cannot be — but its permit travels with it, so the gate
  keeps counting it until it returns. Core query deadlines (03 §8.6) sit
  beneath this and end most bodies sooner.
- **The relay takes a credential only from the repository's own file.** The
  `.mcp.json` walk stops at the first `.git`, and a file not owned by the
  effective user or writable by everyone is passed over: what it names
  receives the host's session and the `Authorization` it carries.
- Optional read-only mode (`--read-only`) for exploration deployments.

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
   `drsg-mcp` did gain a client *transport* — not a client mode: with no
   database named it reads the repository's `.mcp.json`, and when the server
   declared there answers it **relays** the stdio session to it verbatim
   (`compact`-free, message level, so the host sees that server's tools).
   The rule "one process opens a database directly" is unchanged; what
   changed is that a host arriving second now joins rather than fails.
