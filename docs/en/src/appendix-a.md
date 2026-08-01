# Appendix A: JSON-RPC API List

This appendix lists the JSON-RPC 2.0 methods exposed by `drsg serve`. The
`OpenRPC` schema at `crates/dr-strange-web/openrpc.json` is the authoritative
source — the server returns it from `rpc.discover`, and the SDKs are generated
from it. Each method carries an **access** tier (`read`, `write`, or `admin`);
under the single shared token all three require the same token.

## Discovery and database

| Method | Access | Summary |
|---|---|---|
| `rpc.discover` | read | the OpenRPC service description |
| `db.stats` | read | plane/node/edge counts, labels, edge types, indexes, commits, on-disk size |
| `db.catalog` | read | the soft-schema catalog rolled up across every plane |

## Planes

| Method | Access | Summary |
|---|---|---|
| `plane.list` | read | every plane with id, name, counts, and properties |
| `plane.catalog` | read | one plane's soft schema |
| `plane.indexes` | read | the search indexes declared on a plane |
| `plane.history` | read | the time-travel window (native backend only) |
| `plane.create` | admin | create an empty plane |
| `plane.rename` | admin | rename a plane |
| `plane.set_props` | admin | replace a plane's property map |
| `plane.delete` | admin | drop a plane and its contents |

## Nodes and edges

| Method | Access | Summary |
|---|---|---|
| `node.get` | read | one node by id or external key |
| `node.create` | write | add a node with optional key and labels |
| `node.update` | write | patch a node's properties and labels |
| `node.delete` | write | delete a node, cascading its edges |
| `edge.create` | write | add a directed edge between two nodes |
| `edge.update` | write | patch an edge's properties or type |
| `edge.delete` | write | delete one edge |

## Query and retrieval

| Method | Access | Summary |
|---|---|---|
| `plane.neighbors` | read | 1-hop expansion as `{node, edge}` id pairs |
| `plane.query` | read | run a serialized logical plan |
| `plane.cypher` | write | run an openCypher-subset statement (write-gated) |
| `plane.find` | read | text or semantic search over a plane |
| `plane.search` | read | vector top-*k* over a property |
| `plane.hybrid` | read | fused vector + keyword + graph-proximity search |
| `plane.algo` | read | a graph algorithm (pagerank / components / shortest_path / louvain) |
| `plane.ask` | read | natural-language query → plan → run |
| `graph.seed` | read | an initial canvas of nodes plus induced edges |
| `graph.expand` | read | hub-safe 1-hop neighbourhood around a node |

`plane.query`, `plane.neighbors`, `plane.find`, `graph.seed`, and `graph.expand`
accept optional `as_of` (commit sequence) or `as_of_ms` (timestamp) parameters
for time-travel reads (native backend only).

## Indexes and ingestion

| Method | Access | Summary |
|---|---|---|
| `index.ensure` | admin | declare a vector or keyword index on `(label, property)` |
| `digest.run` | write | extract a node/edge proposal from text via the LLM (dry run) |
| `digest.write` | write | write a previously-computed proposal (no LLM call) |

## WebSocket subscription

The `/ws` endpoint answers the same request/response methods and additionally
supports the change feed (these are WebSocket-only):

| Message | Direction | Summary |
|---|---|---|
| `plane.watch` | client → server | subscribe to a plane's changes (optional `label`) |
| `plane.unwatch` | client → server | stop the subscription |
| `plane.change` | server → client | a committed change set `{plane, seq, truncated, changes}` |

## Error codes

| Code | Meaning |
|---|---|
| `-32700` | parse error |
| `-32600` | invalid request |
| `-32601` | method not found |
| `-32602` | invalid params |
| `-32000` | application error (bad plane, dangling endpoint, conflict, …) |
| `-32001` | unauthorized (missing or invalid credential) |
