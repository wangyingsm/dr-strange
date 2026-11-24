# Appendix A: JSON-RPC API List

This appendix is a reference for the JSON-RPC 2.0 methods exposed by `drsg serve`.
The `OpenRPC` schema at `crates/dr-strange-web/openrpc.json` is the authoritative
source — the server returns it from `rpc.discover`, and the SDKs are generated
from it.

Each entry shows the method, its **access** tier (`read` / `write` / `admin`;
under the single shared token all three require the same token), a one-line
summary, and its parameters. A parameter is written `name` type; **`!` marks a
required parameter**. Types are JSON values; `Properties` is the property-map
dialect (`{"$vector":[…]}`, `{"$desc":…,"$value":…}`, `{"$bytes":[…]}`, and
`{"$map":{…}}` for a nested map whose own keys begin with `$`; integers must fit
a signed 64-bit value), and `NodeRef` is a node id or an external key.

**An argument a method cannot act on reads as `-32602`, not `-32000`**, on the
write methods as on the read ones: a vector whose width the index contradicts,
a declared width the property's own vectors contradict, a re-declaration at a
different metric, language or width, an unparsable `$expires_at`, and a plane
that has exhausted its label dictionary or its plane ids. Previously those
answered `-32000`, which is the code for a condition of the store rather than
of the call. A duplicate external key is unchanged: a conflict *is* a condition
of the store.

**A parameter the method does not define is refused**, with `-32602` and the
offending name, rather than ignored: a misspelling is a mistake the caller can
see at the call that made it. The exception is the five methods that accept the
time-travel parameters (`as_of` / `as_of_ms`) — `plane.query`, `plane.neighbors`,
`plane.find`, `graph.seed` and `graph.expand` — which still ignore what they do
not know: serde cannot combine a flattened parameter group with rejecting the
unknown, a limitation of the deserializer rather than a decision.

## Discovery and database

- **`rpc.discover`** · read — the OpenRPC service description. Params: none.
- **`db.stats`** · read — plane/node/edge counts, labels, edge types, indexes, commits, on-disk size, memory (the process's resident set, and the bytes the loaded plugins hold). Params: none.
- **`db.catalog`** · read — the soft-schema catalog across every plane. Params: none.
- **`db.mount`** · admin — serve the planes of another database alongside this one's (issue #44). The directory must already be a database and must sit under `[server] mount_root`; a store another process holds is refused. Params: `path`, `as?` (prefix the mount's plane names).
- **`db.unmount`** · admin — stop serving a mount, releasing its store's lock. `freed` says whether the lock came free before the call returned: a fold in flight holds it until it finishes. An absent name is reported, not an error. Params: `name`.
- **`db.mounts`** · read — what this instance serves: each mount with its directory and planes, and every plane with the source tree it records. Params: none.

## Planes

- **`plane.list`** · read — every plane with id, name, counts, properties. Params: none.
- **`plane.catalog`** · read — one plane's soft schema. Params: `plane` string!.
- **`plane.indexes`** · read — the search indexes declared on a plane; each vector row carries the width of the vectors it holds as `dim` (`null` with `"mixed": true` where they differ) and the width its declaration fixed as `declared_dim` (`null` where none did). Params: `plane` string!.
- **`plane.history`** · read — the time-travel window (native backend only). Params: none.
- **`plane.create`** · admin — create an empty plane. Params: `name` string!, `properties` Properties.
- **`plane.rename`** · admin — rename a plane. Params: `plane` string!, `to` string!.
- **`plane.set_props`** · admin — replace a plane's property map. Params: `plane` string!, `properties` Properties!.
- **`plane.delete`** · admin — drop a plane and its contents. Params: `plane` string!.
- **`plane.attach`** · admin — record a plane this one may ask: one on this instance, or one another drsg serves (`url`). Params: `plane` string!, `name` string!, `url` string, `as` string, `both` boolean.
- **`plane.detach`** · admin — forget an attachment by name (absent is a no-op). Params: `plane` string!, `name` string!.
- **`plane.attachments`** · read — what a plane attaches, as recorded; nothing is contacted. Params: `plane` string!.

## Nodes and edges

- **`node.get`** · read — one node by id or external key. Params: `plane` string!, `id` integer, `key` string.
- **`node.create`** · write — add a node with optional key and labels. A reserved property `$expires_at` (epoch milliseconds) hides the node from every read past that instant (see [Chapter 3](./ai-native.md#a-plane-as-a-cache-expires_at)). Params: `plane` string!, `key` string, `labels` array, `properties` Properties.
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
- **`plane.search`** · read — vector top-*k* over a property; a query the declared index's own width contradicts is refused with `-32602` naming both widths (see [Chapter 3](./ai-native.md#the-indexs-own-width)). An omitted `metric` means the metric the indexes on the property were declared with, cosine only where none says otherwise, and an unknown name is refused; `min_score` answers only hits scoring at least that much, on the same scale as each hit's `score`, applied before `k` is filled (see [Chapter 3](./ai-native.md#a-threshold-not-just-a-rank-min_score)). Params: `plane` string!, `property` string!, `query` array!, `label` string, `k` integer, `metric` string, `min_score` number.
- **`plane.hybrid`** · read — fused vector + keyword + graph-proximity search. Its `metric` follows the same rule as `plane.search`'s; it takes no `min_score`, because the channels fuse normalised scores. Params: `plane` string!, `q` string!, `label` string, `vector_prop` string, `keyword_prop` string, `metric` string, `graph_hops` integer, `graph_decay` number, `w_vector` number, `w_keyword` number, `w_graph` number, `k` integer, `candidates` integer, `provider` string, `embed_model` string.
- **`plane.algo`** · read — a graph algorithm (`pagerank` \| `components` \| `shortest_path` \| `louvain` \| `toposort` \| `cycles`) over a plane or label subset. Params: `plane` string!, `algo` string!, `label` string, `limit` integer, `damping` number, `max_iters` integer, `tolerance` number, `src` integer, `dst` integer, `dir` string, `weight` string, `max_levels` integer, `min_gain` number, `edge_type` string.
- **`plane.ask`** · read — natural-language query → plan → run. Params: `plane` string!, `question` string!, `dry_run` boolean, `max_attempts` integer, `limit` integer, `provider` string, `model` string, `embed_provider` string, `embed_model` string.
- **`graph.seed`** · read — an initial canvas of nodes plus induced edges. Params: `plane` string!, `label` string, `limit` integer, `order` string (`scan` \| `degree` \| `pagerank`, default `scan`), `as_of` integer, `as_of_ms` integer. A ranked `order` returns the highest-scoring nodes rather than the first the scan reaches, and includes their `scores` — prefer `degree` for a skeleton, since PageRank pools rank in sinks.
- **`graph.expand`** · read — hub-safe 1-hop neighbourhood around a node. Params: `plane` string!, `id` integer!, `direction` string, `type` string, `limit` integer, `as_of` integer, `as_of_ms` integer.

## Indexes and ingestion

- **`index.ensure`** · admin — declare a vector or keyword index on `(label, property)`. A vector index may fix `dim`, the width every vector in it must be, which is then enforced from the first write and reported as `declared_dim` by `plane.indexes`; a width the property's existing vectors contradict is refused, as is an unknown `metric`. Params: `plane` string!, `label` string!, `property` string!, `kind` string, `metric` string, `language` string, `dim` integer.
- **`digest.run`** · write — extract a node/edge proposal from text via the LLM (dry run). Params: `plane` string!, `text` string!, `chat` string, `embed` string, `model` string, `embed_model` string, `source` string, `no_embed` boolean, `link` boolean, `concurrency` integer, `chunk_chars` integer, `mode` string (`coarse` \| `fine` \| `super`, default `fine` — see [Chapter 3](./ai-native.md#extraction-precision); `super` costs ~15× the input tokens).
- **`digest.write`** · write — write a previously-computed proposal (no LLM call). A proposed key the plane already owns is skipped by default, since a distillation proposes every entity as new. Params: `plane` string!, `nodes` array!, `edges` array, `on_conflict` string (`error` / `skip` / `update`; default `skip`). Each node is `{key string!, label string, labels array, extra_labels array, properties Properties}` — **all three label spellings are accepted and unioned**, so a proposal reviewed from `digest.run` keeps the labels it was given; a node naming none lands under `Entity`. Each edge is `{src string!, dst string!, type string!, properties Properties}`.
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
