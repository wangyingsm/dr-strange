# MCP

`drsg-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server
that embeds Dr Strange and exposes the database to LLM agents as a set of tools.
An agent in a compatible host can search, traverse, query, run algorithms, ask
natural-language questions, and ingest documents — and write to the graph —
directly, without bespoke integration code.

## How Dr Strange fits an agent

MCP is itself JSON-RPC 2.0 — the same protocol the web backend speaks — so an MCP
server is a first-class surface rather than an adapter. `drsg-mcp` embeds
`dr-strange-core` and opens the database **in-process** (as the CLI does), then
serves the protocol over **stdio**: the host launches the process and exchanges
JSON-RPC messages over its standard input and output. Logs are written to stderr
and a rolling file, never to stdout, which carries the protocol.

## Running and configuring

`drsg-mcp` is a separate binary. Install a release with one line ([Chapter
2](./getting-started.md#installing-a-released-binary)):

```console
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin drsg-mcp
```

On Windows, in PowerShell:

```console
PS> & ([scriptblock]::Create((irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1))) -Bin drsg-mcp
```

It can equally be built from source with `cargo build --release -p
dr-strange-mcp`. It takes the database as `--db <path>` or as a bare argument,
else `$DRSG_DB`, else `graph.drsg`; `--help` and `--version` print and exit:

```console
$ drsg-mcp --db /path/to/graph.drsg
$ drsg-mcp /path/to/graph.drsg          # the same thing, short
```

The database must already exist — this server never creates one. Build it with
`drsg digest <dir> --apply --db <path>`, or with `drsg init` in a repository. An
empty database would answer every question with "nothing found", which reads
exactly like a digest that went wrong, so a path that isn't there is an error
instead.

**With no database named, it looks for a server first.** The nearest
`.mcp.json` is read — walking up, as git finds its own directory — and if it
declares a drsg server that answers, this process **relays** the host's
session to it instead of opening anything. So in a repository prepared by
`drsg init` (which digests the tree, starts a `drsg serve … watch`, and writes
that server's URL into `.mcp.json`), a stdio-only host reaches the process that
already holds the database, with a plane synced to the repository's commits.

The relay forwards messages as they are, in both directions, so the host sees
that server's tool set — including tools a newer server has and this binary has
never heard of. Naming a database (`--db`, `$DRSG_DB`) skips the search: a
caller who says which graph they want is not asking to be sent to another one.
Nothing answering, no `.mcp.json`, or no drsg server in it, and the database is
opened here as before.

It is normally launched by the host rather than run by hand. A host configures it
by command, arguments, and environment — the environment carries any LLM provider
keys the graph tools need:

```json
{
  "mcpServers": {
    "dr-strange": {
      "command": "drsg-mcp",
      "args": ["--db", "/path/to/graph.drsg"],
      "env": {
        "OPENAI_API_KEY": "sk-...",
        "DEEPSEEK_API_KEY": "...",
        "DASHSCOPE_API_KEY": "..."
      }
    }
  }
}
```

A ready-to-edit copy of this configuration is provided at
`crates/dr-strange-mcp/mcp.json`; set `command` to `drsg-mcp` (on the `PATH`) or
an absolute path to the binary, point `args` at your database, and supply only
the provider keys you use.

Because it opens the database in-process, **one process at a time** may open a
given database directly — a `drsg-mcp`, a `drsg` command, or a `drsg serve`, but
not two at once. This is enforced, not advisory: the second open fails with a
clear error rather than corrupting the database.

That is the rule the relay above exists to keep out of your way, and it still
binds whenever the relay does not apply. Two editors open on the same project
each spawn their own `drsg-mcp`: with a watch server running, both relay to it
and share one database; with none, the first opens the file and the second
refuses to start. For that second case — several agents that need to share one
memory — see the next section.

## Sharing across agents: `/mcp` on `drsg serve`

`drsg-mcp` embeds by design: point a host at a path and it works, with
nothing to run and nothing to configure. That's the right answer for one
agent, but it's also *why* two hosts can't share a database directly — each
one opens the file itself.

`drsg serve` (arch/08) already solves the "one writer, several clients"
problem for the JSON-RPC API and the dashboard. It exposes the identical
tool set at `POST /mcp`, over MCP's Streamable HTTP transport, so several
agent hosts can point at the same server instead of each embedding their own
copy of the database:

```json
{
  "mcpServers": {
    "dr-strange": {
      "url": "http://127.0.0.1:7700/mcp",
      "headers": { "Authorization": "Bearer <DRSG_TOKEN>" }
    }
  }
}
```

(Check your host's docs for its exact remote-MCP-server config shape — the
`url`/`headers` fields above are illustrative, not universal.)

`/mcp` is gated exactly like `/rpc`: with `DRSG_TOKEN` unset, only the
same-origin browser UI is trusted, and **every programmatic client is
refused, reads included** — a zero-config desktop install shouldn't quietly
expose an open API on localhost. Set `DRSG_TOKEN` on the server and pass it
as a bearer token to reach `/mcp` from anywhere else, including another
agent host.

One limit worth knowing before putting this behind a hostname: the MCP
transport validates the inbound `Host` header against a loopback-only list
(`localhost`, `127.0.0.1`, `::1`) to blunt DNS-rebinding attacks on locally
running servers, and answers **403** to anything else. The check earns its
keep here — a tokenless server trusts its own same-origin UI, which is what
rebinding sets out to impersonate — but it does mean `/mcp` answers at
`http://127.0.0.1:7700/mcp` and not at `https://memory.example.com/mcp`,
where `/rpc` on the same server would. Agent hosts on the same machine, the
case ROADMAP §10 is about, are unaffected.

Every session gets its own `DrStrange` instance, but they all share the one
`Database` the server opened — a write from one session is visible to every
other, immediately, the same way two browser tabs against `/rpc` already
are. That's the whole point: `write_gate` inside the core serializes
concurrent writers so this is safe, not just convenient.

`digest`'s LLM calls still spend the server process's provider keys (never a
tool parameter, remote or local); `write_nodes`/`write_edges` keep their
per-call batch atomicity, since the tool code runs in the same process
against the same `Database` either way — nothing here proxies to `/rpc` or
reshapes what a tool does. The `[digest]` config section steers the `digest`
tool exactly as it steers `digest.run` over `/rpc`: lowering `concurrency` to
stay under a provider's rate limit applies to both surfaces, not one of them.
(The embedded `drsg-mcp` binary has no config file and keeps the built-in
defaults.)

One capability differs by transport rather than by configuration. `digest`
accepts a `path` to a document the server reads — Word, PowerPoint, Excel,
OpenDocument, RTF, EPUB, CSV, PDF, Markdown or plain text — and **only the
stdio server honours it**. That server runs on your own machine as your own
user, so it reads exactly what the agent could already open for itself. A
shared `drsg serve` refuses it: reading any path the caller names would let an
authenticated remote agent pull arbitrary server files into the graph and query
them back out. Over `/mcp`, send the document as `text` instead.

### Session lifetime

A host that exits cleanly sends `DELETE /mcp` and its session goes away at
once. A host that is `SIGKILL`ed — an editor restarting its MCP child, say —
sends nothing, so the server reclaims that session on a timer instead: **10
minutes idle**, or **60 seconds** with no `initialize` after the session is
created. The session's worker task, its `DrStrange`, and its buffered
messages all go with it.

The idle window is ten minutes rather than five for a specific reason: the
transport counts a *running* tool as idle, because its keep-alive timer is only
reset by traffic and a tool call sends nothing between dispatch and its result.
On an otherwise quiet session, a tool that runs longer than the window is torn
down mid-flight. Ten minutes clears any realistic `digest`; if you routinely run
longer ones, keep the session busy or expect to retry.

What survives a reclaim is one map entry per dead session. It is tens of bytes,
but it is not inert: the next request on that session id is answered **500**
rather than the **404** the spec calls for, so a client that would have
re-initialized does not, and that session stays broken until the host restarts.
Both of these are transport-level and fixed upstream rather than here. Closing
sessions you script in a loop avoids the whole area.

### Tool concurrency

Tool calls are bounded separately from HTTP requests, at **16 at once** across
the whole process (or `max_concurrent`, if that is lower). The two ceilings are
not the same thing: `max_concurrent` counts requests, most of which are cheap,
while every tool call is a full-graph scan, a bulk write, or an LLM-fanning
digest. The transport also answers a tool call as soon as it is *queued*, so the
request ceiling has already released the call before the work begins and cannot
bound it. Excess calls queue rather than fail — a busy server makes an agent
wait, it does not turn it away.

## The tools

| Tool | Kind | Purpose |
|---|---|---|
| `list_planes` | read | list planes with node/edge counts |
| `describe_plane` | read | a plane's soft schema (labels, properties, edge types) |
| `get_node` | read | fetch one node by id or external key |
| `search` | read | semantic lookup — embeds the query, returns the *k* nearest nodes |
| `context` | read | one symbol's whole neighborhood on a digested code plane — the primary agent verb |
| `describe` | read | one symbol's properties, the lightweight node-only view |
| `grep` | read | text search over the watched source tree — literal or regex, scoped by `path`, with context lines; each hit names the symbol it falls in |
| `trace` | read | the shortest recorded call path between two symbols |
| `impact` | read | everything reaching a symbol, grouped by distance |
| `fathom` | read | the makeup of the region around a symbol — labels, edge types, hubs |
| `snippet` | read | a symbol's source text, or a range of a file (`path:start-end`); says which symbol a range opens in and how to read on |
| `traverse` | read | neighborhood expansion from a node (1+ hops) |
| `query` | read | run a serialized logical plan |
| `cypher` | read / write | run an openCypher-subset statement — the escape hatch for what no verb anticipated |
| `algo` | read | a graph algorithm (pagerank / components / shortest_path / louvain) |
| `hybrid` | read | fused vector + keyword + graph-proximity search |
| `ask` | read | a natural-language question, compiled to a plan and run |
| `write_nodes` | write | create nodes (batched) |
| `write_edges` | write | create edges (batched) by endpoint keys |
| `create_plane` | write | create an empty plane |
| `drop_plane` | write | delete a plane and its contents (requires confirmation) |
| `digest` | write | ingest a document (dry-run by default; `mode` sets extraction precision) |

## Mapping to the rest of the system

The tools are the same operations available through the CLI and the JSON-RPC API,
adapted to an agent's needs: `search` / `traverse` / `query` / `cypher` / `algo`
/ `hybrid` / `ask` mirror the query and retrieval surface of [Chapter
4](./query-language.md) and [Chapter 3](./ai-native.md); `write_nodes` /
`write_edges` / `create_plane` / `drop_plane` / `digest` mirror the write and
ingestion surface. Each is grounded in the plane's soft schema, which
`describe_plane` exposes so an agent can discover a graph before acting on it.

## Safety

- **Provider keys are read from the server's environment, never from tool
  parameters** — an agent cannot exfiltrate or supply a key through a call.
- **Reads are non-destructive.** `ask` in particular compiles to a read-only plan
  and cannot mutate the graph.
- **Destructive writes are guarded.** `drop_plane` requires an explicit
  confirmation flag, and `digest` defaults to a dry run that returns the proposed
  nodes and edges for inspection rather than writing them.
- **A mirrored plane is not rewritten by hand.** A plane that `digest` or
  `serve watch` keeps in step with a source tree records the commit it
  reflects, and `cypher` refuses `CREATE`/`MERGE`/`SET`/`REMOVE`/`DELETE` on
  it: the next fold reconciles the plane against the tree, overwriting edits
  to parser-owned nodes and re-creating deletions, so such a write would be
  undone without a trace. `write_nodes` / `write_edges` still work there — a
  fold leaves nodes it does not own alone — and a plane of the agent's own
  takes every statement.

On those same mirrored planes `cypher` answers a `RETURN n` in the compact
text the agent verbs speak — a count, the synced commit, one line per node —
rather than a JSON record apiece; a projection is a table everywhere. And a
statement that fails to parse comes back with the grammar of the clauses it
reached for, so the tool listing carries the intent and two examples rather
than the whole language on every turn.

## Example: an agent workflow

A typical agent session composes the tools:

1. `describe_plane` to learn the labels, properties, and edge types in scope.
2. `search` or `hybrid` to retrieve relevant nodes, then `traverse` to gather
   their neighborhood — grounded context for the model.
3. `ask` or `cypher` to answer a specific question against the graph.
4. `digest` (dry run, then applied) to fold new source material into the graph as
   entities and relations.

Because the store is a graph, the agent can move from retrieval into traversal in
a single session — the GraphRAG loop of [Chapter 1](./what-is-dr-strange.md),
driven end to end by the model.
