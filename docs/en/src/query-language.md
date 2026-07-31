# Query Language

Dr Strange answers queries through a serializable **logical plan** — a small,
explicit pipeline of operators (scan, seek, expand, filter, sort, limit,
vector-top-k). You can build that plan directly, or write it in an
**openCypher-subset** language that compiles to the same plan.

## Cypher subset

```text
MATCH (p:Person)-[:KNOWS]->(q:Person)
WHERE p.age >= 18
RETURN q
LIMIT 50
```

Reads (`MATCH … RETURN`) return a subgraph; writes (`CREATE`, `MERGE`, `SET`,
`REMOVE`, `DELETE`) mutate and report change counts. Values can be passed as
`$name` parameters instead of being interpolated into the text.

## Similarity search in a query

```text
SEARCH (d:Doc) ON embedding NEAR "some text" TOPK 10 RETURN d
```

`NEAR "text"` embeds the text server-side; `NEAR $vec` takes a literal vector.

## Graph algorithms

Read-only, transient runs over one snapshot of a plane (whole-plane or
label-scoped): PageRank, weakly-connected components, weighted shortest path,
and Louvain community detection.

## Time-travel

Any read can be pinned to a past point with an **AS OF** address — a commit
sequence or a timestamp — to see the graph exactly as it was.

## Sections (draft)

- The logical plan: operators and how a query becomes one
- Cypher subset: `MATCH` / `WHERE` / `RETURN` / `ORDER BY` / `SKIP` / `LIMIT`
- Writes: `CREATE` / `MERGE` / `SET` / `REMOVE` / `DELETE`, and `$params`
- `SEARCH … NEAR … TOPK`: text vs. literal-vector similarity
- Expressions, predicates, and property access
- Graph algorithms (`plane.algo`) and their options
- Time-travel (`AS OF <seq | timestamp>`) and its guarantees
- Running plans directly vs. through the language (parity)
