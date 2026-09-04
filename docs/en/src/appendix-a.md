# Appendix A: JSON-RPC API List

This appendix is a reference for the JSON-RPC 2.0 methods exposed by `drsg serve`.
The `OpenRPC` schema at `crates/dr-strange-web/openrpc.json` is the authoritative
source — the server returns it from `rpc.discover`, and the SDKs are generated
from it.

Each entry shows the method, its **access** tier (`read` / `write` / `admin`;
under the single shared token all three require the same token), a one-line
summary, and its parameters. A parameter is written `name` type; **`!` marks a
required parameter**. Types are JSON values; `Properties` is the property-map
dialect (`{"$vector":[…]}`, `{"$desc":…,"$value":…}`), and `NodeRef` is a node id
or an external key.

## Discovery and database

- **`rpc.discover`** · read — the OpenRPC service description. Params: none.
- **`db.stats`** · read — plane/node/edge counts, labels, edge types, indexes, commits, on-disk size, memory (the process's resident set, and the bytes the loaded plugins hold). Params: none.
- **`db.catalog`** · read — the soft-schema catalog across every plane. Params: none.

## Planes

- **`plane.list`** · read — every plane with id, name, counts, properties. Params: none.
- **`plane.catalog`** · read — one plane's soft schema. Params: `plane` string!.
- **`plane.indexes`** · read — the search indexes declared on a plane. Params: `plane` string!.
- **`plane.history`** · read — the time-travel window (native backend only). Params: none.
- **`plane.create`** · admin — create an empty plane. Params: `name` string!, `properties` Properties.
- **`plane.rename`** · admin — rename a plane. Params: `plane` string!, `to` string!.
- **`plane.set_props`** · admin — replace a plane's property map. Params: `plane` string!, `properties` Properties!.
- **`plane.delete`** · admin — drop a plane and its contents. Params: `plane` string!.

## Nodes and edges

- **`node.get`** · read — one node by id or external key. Params: `plane` string!, `id` integer, `key` string.
- **`node.create`** · write — add a node with optional key and labels. Params: `plane` string!, `key` string, `labels` array, `properties` Properties.
- **`node.update`** · write — patch properties (`set`/`unset`) and labels. Params: `plane` string!, `id` integer, `key` string, `set` Properties, `unset` array, `labels` array.
- **`node.delete`** · write — delete a node, cascading its edges. Params: `plane` string!, `id` integer, `key` string.
- **`edge.create`** · write — add a directed edge between two nodes. Params: `plane` string!, `src` NodeRef!, `dst` NodeRef!, `type` string!, `properties` Properties.
- **`edge.update`** · write — patch properties (`set`/`unset`) or the type. Params: `plane` string!, `edge` integer!, `set` Properties, `unset` array, `type` string.
- **`edge.delete`** · write — delete one edge. Params: `plane` string!, `edge` integer!.

## Query and retrieval

- **`plane.neighbors`** · read — 1-hop expansion as `{node, edge}` id pairs. Params: `plane` string!, `id` integer!, `direction` string, `type` string, `as_of` integer, `as_of_ms` integer.
- **`plane.query`** · read — run a serialized logical plan. Params: `plane` string!, `plan` object!, `as_of` integer, `as_of_ms` integer.
- **`plane.cypher`** · write — run an openCypher-subset statement (write-gated). Params: `plane` string!, `query` string!, `embed` string, `params` object.
- **`plane.find`** · read — text or semantic search over a plane. Params: `plane` string!, `q` string!, `limit` integer, `semantic` boolean, `provider` string, `embed_model` string, `as_of` integer, `as_of_ms` integer.
- **`plane.search`** · read — vector top-*k* over a property. Params: `plane` string!, `property` string!, `query` array!, `label` string, `k` integer, `metric` string.
- **`plane.hybrid`** · read — fused vector + keyword + graph-proximity search. Params: `plane` string!, `q` string!, `label` string, `vector_prop` string, `keyword_prop` string, `metric` string, `graph_hops` integer, `graph_decay` number, `w_vector` number, `w_keyword` number, `w_graph` number, `k` integer, `candidates` integer, `provider` string, `embed_model` string.
- **`plane.algo`** · read — a graph algorithm over a plane or label subset. Params: `plane` string!, `algo` string!, `label` string, `limit` integer, `damping` number, `max_iters` integer, `tolerance` number, `src` integer, `dst` integer, `dir` string, `weight` string, `max_levels` integer, `min_gain` number.
- **`plane.ask`** · read — natural-language query → plan → run. Params: `plane` string!, `question` string!, `dry_run` boolean, `max_attempts` integer, `limit` integer, `provider` string, `model` string, `embed_provider` string, `embed_model` string.
- **`graph.seed`** · read — an initial canvas of nodes plus induced edges. Params: `plane` string!, `label` string, `limit` integer, `order` string (`scan` \| `degree` \| `pagerank`, default `scan`), `as_of` integer, `as_of_ms` integer. A ranked `order` returns the highest-scoring nodes rather than the first the scan reaches, and includes their `scores` — prefer `degree` for a skeleton, since PageRank pools rank in sinks.
- **`graph.expand`** · read — hub-safe 1-hop neighbourhood around a node. Params: `plane` string!, `id` integer!, `direction` string, `type` string, `limit` integer, `as_of` integer, `as_of_ms` integer.

## Indexes and ingestion

- **`index.ensure`** · admin — declare a vector or keyword index on `(label, property)`. Params: `plane` string!, `label` string!, `property` string!, `kind` string, `metric` string, `language` string.
- **`digest.run`** · write — extract a node/edge proposal from text via the LLM (dry run). Params: `plane` string!, `text` string!, `chat` string, `embed` string, `model` string, `embed_model` string, `source` string, `no_embed` boolean, `link` boolean, `concurrency` integer, `chunk_chars` integer, `mode` string (`coarse` \| `fine` \| `super`, default `fine` — see [Chapter 3](./ai-native.md#extraction-precision); `super` costs ~15× the input tokens).
- **`digest.write`** · write — write a previously-computed proposal (no LLM call). Params: `plane` string!, `nodes` array!, `edges` array.
- **`plane.vectorize`** · write — embed every node in a plane (unchanged texts are skipped) and ensure a vector index on `embedding` per label; the provider key comes from the server's environment. Params: `plane` string!, `embed` string, `embed_model` string, `metric` string.

## Plugins

- **`plugin.list`** · read — the installed preprocessor plugins, the same records `drsg plugin list --json` prints. Params: none.
- **`plugin.catalog`** · read — the official catalog, read from the extensions repository's `catalog.json` rather than compiled into the binary, so a plugin release needs no drsg release. Returns `{stale, schema, source, plugins}`; each entry carries `name`, `version`, `claims`, `url`, `sha256` and `compat` (`ok`, `needs_host`, `other_contract`), joinable against `plugin.list`. Cached for an hour; `stale: true` means the fetch failed and this is the last copy the server kept. Params: none.
- **`plugin.install`** · write — download, validate, hash-pin and store a plugin from an `http(s)` URL (server-local paths are refused over RPC). Params: `url` string!.
- **`plugin.remove`** · write — uninstall a plugin by name. Params: `name` string!.

## WebSocket subscription

The `/ws` endpoint answers the same request/response methods and additionally
supports the change feed (these are WebSocket-only):

- **`plane.watch`** · client → server — subscribe to a plane's changes. Params: `plane` string!, `label` string.
- **`plane.unwatch`** · client → server — stop the subscription. Params: none.
- **`plane.change`** · server → client — a committed change set. Fields: `plane`, `seq`, `truncated`, `changes`.

## Error codes

| Code | Meaning |
|---|---|
| `-32700` | parse error |
| `-32600` | invalid request |
| `-32601` | method not found |
| `-32602` | invalid params |
| `-32000` | application error (bad plane, dangling endpoint, conflict, …) |
| `-32001` | unauthorized (missing or invalid credential) |
