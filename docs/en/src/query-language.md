# Query Language

Every read and write in Dr Strange executes as a **logical plan** — an explicit
pipeline of operators. A plan may be authored directly as a serializable
structure, or written in an **openCypher-subset** language that compiles to the
same plan. The two are equivalent; the language is a surface over the plan.

This chapter explains what each construct is for. [Appendix B](./appendix-b.md)
states the complete grammar — every clause, every default, and everything that
is deliberately not supported.

## The logical plan

A plan is a **source** followed by a sequence of **steps**. Each row flows
through the pipeline carrying a current node and the path that produced it.

Sources:

| Source | Rows produced |
|---|---|
| `ScanAll` | every node in the plane |
| `ScanLabel(label)` | nodes carrying `label` |
| `SeekKeys(keys)` | nodes resolved from external keys |
| `VectorTopK{…}` | the *k* nodes nearest a query vector |
| `KeywordTopK{…}` | the *k* nodes best matching a text query (BM25) |
| `Hybrid{…}` | the *k* nodes ranked by fused vector + keyword + proximity |
| `Algo{…}` | the nodes a graph algorithm produced, each carrying its result |

Every source yields rows the remaining steps then process, so retrieval and
algorithms are not terminal operations: a keyword hit, a fused hit, or a
PageRank result can be traversed from, filtered, sorted, and bounded like any
scanned node.

Steps:

| Step | Effect |
|---|---|
| `Expand{dir, edge_type}` | traverse one hop (direction, optional edge type) |
| `ExpandVar{…}` | traverse a variable number of hops |
| `Filter(expr)` | keep rows whose predicate holds |
| `Distinct` | deduplicate by current node |
| `Sort(keys)` | order rows |
| `Skip(n)` / `Limit(n)` | offset / bound the result |

The plan is serializable. `MATCH (:Person)-[:KNOWS]->(q) RETURN q LIMIT 50`
corresponds to:

```json
{
  "source": { "ScanLabel": "Person" },
  "steps": [
    { "Expand": { "dir": "Out", "edge_type": "KNOWS" } },
    { "Limit": 50 }
  ]
}
```

A plan in this form runs directly:

```console
$ drsg --db graph.drsg query - --plane social < plan.json
```

## The Cypher subset

Reads are expressed with `MATCH … RETURN`, optionally refined by `WHERE`,
`ORDER BY`, `SKIP`, `LIMIT`, and `DISTINCT`:

```text
MATCH (p:Person)-[:KNOWS]->(q:Person)
WHERE p.age >= 18
RETURN q
ORDER BY q.name
LIMIT 50
```

A `MATCH` is a node pattern, optionally chained through relationship patterns
(`-[:TYPE]->`, `<-[:TYPE]-`, or undirected). `WHERE` predicates combine property
comparisons (`=`, `<>`, `<`, `<=`, `>`, `>=`), label tests (`n:Label`), set
membership (`x IN [a, b]`), and the boolean operators `AND`, `OR`, `NOT`. A read
returns the matched subgraph — the nodes and edges on every matching path, not
merely the final nodes.

### Anchoring on a known entity

`key(n)` reads a node's external key — the stable identifier it was created
with. It is an ordinary term, usable anywhere an expression is, but an equality
(or `IN`) on the query's *first* variable is compiled into a `SeekKeys` source:
an index lookup rather than a scan followed by a filter.

```text
MATCH (n:Doc) WHERE key(n) = "paper-42" RETURN n
MATCH (n) WHERE key(n) IN ["ada", "alan"] RETURN n
```

This is the shape that matters for a graph whose identity lives in the key
rather than in a property — the common case for LLM-ingested material — and it
anchors a traversal from a specific entity:

```text
MATCH (n)-[:CITES]->(p:Paper)
WHERE key(n) = "paper-42"
RETURN p
```

## Writes

The write clauses mutate the plane and report change counts rather than rows:

| Clause | Effect |
|---|---|
| `CREATE` | create nodes and edges |
| `MERGE` | match an existing pattern or create it |
| `SET` | add or overwrite properties or labels |
| `REMOVE` | remove properties or labels |
| `DELETE` | delete a node or edge |
| `DETACH DELETE` | delete a node together with its incident edges |

```text
CREATE (a:Person {name:"Ada"})
MERGE (b:Person {name:"Alan"})
CREATE (a)-[:KNOWS {since: 1936}]->(b)
```

Values may be supplied as `$name` parameters rather than interpolated into the
query text, which keeps the query stable and avoids escaping:

```text
MATCH (p:Person) WHERE p.age >= $min RETURN p
```

## Similarity search in a query

The `SEARCH` clause makes similarity a source of rows, so a semantic lookup and a
traversal compose in one statement:

```text
SEARCH (d:Doc) ON embedding NEAR "how does time-travel work" TOPK 10 RETURN d
```

`ON <property>` selects the vector property and may be omitted — every `NEAR`
defaults to `embedding`, the property the document-ingestion pipeline writes, so
`SEARCH (d:Doc) NEAR "…"` is the short form. `TOPK <k>` bounds the result. A text
argument (`NEAR "…"`) is embedded server-side; a literal vector (`NEAR $vec`)
requires no provider. This clause compiles to a `VectorTopK` source, from which
traversal may continue:

```text
SEARCH (d:Doc) ON embedding NEAR "time travel" TOPK 10
-[:CITES]->(p:Paper)
WHERE p.year >= 2020
RETURN p
```

## Keyword search in a query

The same verb searches words instead of meaning when `NEAR` is replaced by
`MATCHING`. It compiles to a `KeywordTopK` source over the BM25 index declared
on that `(label, property)` pair, and each row carries its relevance as
`score()`:

```text
SEARCH (d:Doc) ON body MATCHING "graph database" TOPK 10
RETURN d ORDER BY score() DESC
```

Both a label and `ON <property>` are required here. The keyword index is
declared per `(label, property)`, so there is nothing to search without them —
and unlike the vector property, keyword properties follow no convention worth
defaulting to. Unlike vector search, which falls back to an exact scan, keyword
search returns nothing when no index is declared.

## Hybrid retrieval in a query

`HYBRID` fuses up to three ranked channels into a single ordering. Each channel
is optional (at least one of `VECTOR` or `KEYWORD` is required), may carry a
`WEIGHT`, and they may appear in any order. Only the channel's defining part is
required — `HOPS` for `GRAPH`, a query for the other two — so `VECTOR NEAR "…"`
and `GRAPH HOPS 2` are the short forms; the vector property defaults to
`embedding` and the per-hop decay to `0.5`, matching the RPC, MCP and CLI:

```text
HYBRID (d:Doc)
  VECTOR ON embedding NEAR "graph database internals" METRIC cosine WEIGHT 1.0
  KEYWORD ON body MATCHING "LSM storage engine" WEIGHT 1.0
  GRAPH HOPS 2 DECAY 0.5 WEIGHT 0.5
  CANDIDATES 100 TOPK 10
RETURN d ORDER BY score() DESC
```

This is the `plane.hybrid` retrieval of [Chapter 3](./ai-native.md) expressed in
the language, running through the same fusion engine: each channel is
min-max-normalized before the weighted sum, and `score()` carries the fused
result.

## Graph algorithms

Graph algorithms are read-only, transient computations over a single snapshot of
a plane. They are available as a query source, so their output feeds the rest of
the pipeline:

```text
CALL pagerank(damping: 0.85, iterations: 20) ON (n:Paper)
RETURN n ORDER BY score() DESC LIMIT 10

CALL shortest_path(from: "ada", to: "alan", dir: "both") ON (n)
RETURN n

CALL components() ON (n:Doc) RETURN n
CALL louvain(max_levels: 10) ON (n:Doc) RETURN n
```

`ON (v[:Label])` does double duty: it scopes the algorithm to a label's induced
subgraph (omit the label for the whole plane) and binds the variable the rest of
the query names. Every argument is optional and defaults to the engine's own,
and an unknown algorithm or argument is an error rather than a silently ignored
setting.

Because the row model carries one current node, each algorithm reports its
per-node result through the score channel:

| Algorithm | Row order | `score()` |
|---|---|---|
| `pagerank` | most important first | the rank |
| `components` | grouped by component | a dense 0-based component index |
| `louvain` | grouped by community | a dense 0-based community index |
| `shortest_path` | source → target | the node's position along the path |

`shortest_path` takes its endpoints as external keys (a string) or node ids (a
whole number), optionally a `dir` of `out`/`in`/`both` and a `weight` naming a
numeric edge property. An unknown endpoint or an unreachable target yields no
rows rather than an error.

Since an algorithm is a source, its result composes:

```text
CALL pagerank() ON (n:Paper)
-[:CITES]->(q:Paper)
WHERE q.year >= 2020
RETURN q ORDER BY score() DESC LIMIT 10
```

The same algorithms are also available as a direct command, which reports the
raw result rather than a row stream:

```console
$ drsg --db graph.drsg algo pagerank      --plane social --top 10
$ drsg --db graph.drsg algo components    --plane social
$ drsg --db graph.drsg algo shortest-path --plane social --src 1 --dst 42
$ drsg --db graph.drsg algo louvain       --plane social
```

## Time-travel

Any read may be pinned to a past point — a commit sequence or a timestamp — and
observe the graph exactly as it was then. It is inherently read-only: it selects
the snapshot a query reads, and cannot alter history.

In the language this is the `AS OF` clause, written last so it reads as a
modifier over the whole query. It accepts a commit sequence, an RFC-3339
instant, or `TIME` followed by unix-epoch milliseconds, and it applies to every
source — a scan, a retrieval seed, or an algorithm:

```text
MATCH (p:Paper)-[:CITES]->(q:Paper) RETURN q LIMIT 10 AS OF 41337

SEARCH (d:Doc) ON body MATCHING "outage" TOPK 5
RETURN d AS OF "2026-07-01T00:00:00Z"

CALL pagerank() ON (n:Paper) RETURN n AS OF TIME 1782864000000
```

The same address is available outside the language: as an `as_of` (commit
sequence) or `as_of_ms` (timestamp) argument on the read methods of the RPC API
and the SDKs, as `PlaneHandle::as_of(…)` in the embedded API, and as the
**Time-travel** slider and historical search in the dashboard. Every form uses
"at or before" semantics: a value between two commits resolves to the latest
commit not after it.

A time-travelling read cannot use the vector index, which is built from the
latest commit; its similarity searches scan the pinned snapshot instead —
correct, but unindexed. Time-travel requires the native backend.

The queryable window is reported by `plane.history` as an oldest/latest pair of
commit sequences. History is retained without bound by default; a retention
window may be configured to bound it, in which case a read older than the window
is rejected.

Because a change event carries the commit sequence at which it landed, an agent
can read `as_of(seq)` and `as_of(seq - 1)` to reconstruct the exact before and
after of any committed change (see [Chapter 6](./sdk.md)).
