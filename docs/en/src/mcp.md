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
dr-strange-mcp`. It takes the database path as its first argument, else
`$DRSG_DB`, else `graph.drsg`:

```console
$ drsg-mcp /path/to/graph.drsg
```

It is normally launched by the host rather than run by hand. A host configures it
by command, arguments, and environment — the environment carries any LLM provider
keys the graph tools need:

```json
{
  "mcpServers": {
    "dr-strange": {
      "command": "drsg-mcp",
      "args": ["/path/to/graph.drsg"],
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

Because it opens the database in-process, `drsg-mcp` should not run against a
database a `drsg serve` currently holds open.

## The tools

| Tool | Kind | Purpose |
|---|---|---|
| `list_planes` | read | list planes with node/edge counts |
| `describe_plane` | read | a plane's soft schema (labels, properties, edge types) |
| `get_node` | read | fetch one node by id or external key |
| `search` | read | vector similarity — the *k* nearest nodes |
| `traverse` | read | neighborhood expansion from a node (1+ hops) |
| `query` | read | run a serialized logical plan |
| `cypher` | read | run an openCypher-subset statement |
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
