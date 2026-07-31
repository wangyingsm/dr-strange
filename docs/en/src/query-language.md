# Query Language

Every read and write in Dr Strange executes as a **logical plan** — an explicit
pipeline of operators. A plan may be authored directly as a serializable
structure, or written in an **openCypher-subset** language that compiles to the
same plan. The two are equivalent; the language is a surface over the plan.

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
comparisons (`=`, `<>`, `<`, `<=`, `>`, `>=`), label tests (`n:Label`), and the
boolean operators `AND`, `OR`, `NOT`. A read returns the matched subgraph — the
nodes and edges on every matching path, not merely the final nodes.

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

`ON <property>` selects the vector property; `TOPK <k>` bounds the result. A text
argument (`NEAR "…"`) is embedded server-side; a literal vector (`NEAR $vec`)
requires no provider. This clause compiles to a `VectorTopK` source, from which
`MATCH`-style traversal may continue.

## Graph algorithms

Graph algorithms are separate from the query language: they are read-only,
transient computations over a single snapshot of a plane, invoked by name and
returning a result set rather than mutating the graph. Each runs over the whole
plane or, with a label, over that label's induced subgraph.

```console
$ drsg --db graph.drsg algo pagerank      --plane social --top 10
$ drsg --db graph.drsg algo components    --plane social
$ drsg --db graph.drsg algo shortest-path --plane social --src 1 --dst 42
$ drsg --db graph.drsg algo louvain       --plane social
```

- **PageRank** — importance scores, most-important first (with damping,
  iteration, and tolerance controls).
- **Connected components** — the weakly-connected component of every node
  (represented by the smallest id in the component).
- **Shortest path** — a weighted shortest path between two nodes, following a
  chosen direction, optionally weighted by a numeric edge property.
- **Louvain** — community assignments by modularity optimization.

## Time-travel

Any read may be pinned to a past point — a commit sequence or a timestamp — and
observe the graph exactly as it was then. This is a **read option**, not a
language clause, and it is inherently read-only: it selects the snapshot a query
reads, and cannot alter history.

Time-travel is expressed as an `as_of` (commit sequence) or `as_of_ms`
(timestamp) argument on the read methods of the RPC API and the SDKs, as
`PlaneHandle::as_of(…)` in the embedded API, and as the **Time-travel** slider
and historical search in the dashboard. Both forms use "at or before" semantics:
a value between two commits resolves to the latest commit not after it.

The queryable window is reported by `plane.history` as an oldest/latest pair of
commit sequences. History is retained without bound by default; a retention
window may be configured to bound it, in which case a read older than the window
is rejected.

Because a change event carries the commit sequence at which it landed, an agent
can read `as_of(seq)` and `as_of(seq - 1)` to reconstruct the exact before and
after of any committed change (see [Chapter 6](./sdk.md)).
