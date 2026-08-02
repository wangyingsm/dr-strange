# AI Native

This chapter documents the capabilities that distinguish Dr Strange as a
database designed *for* AI workloads: native vector and keyword indexes, fused
hybrid retrieval, natural-language querying, and LLM-driven document ingestion.
Time-travel and the change feed — equally central to agent workloads — are
covered in [Chapter 4](./query-language.md) and referenced at the end of this
chapter.

## Vector properties and the HNSW index

A property whose value is a vector is an embedding, indexed natively so that
similarity search executes inside the query engine rather than in an external
service.

An index is declared on a `(label, property)` pair with a distance metric. The
engine builds an [HNSW](https://arxiv.org/abs/1603.09320) index over every node
of that label carrying that vector property, and keeps it coherent as writes
commit:

```console
$ drsg --db graph.drsg index ensure Doc embedding --plane social
$ drsg --db graph.drsg index ensure Doc embedding --plane social --metric dot
```

The metric is `cosine` (default), `dot`, or `l2`. Declaration is idempotent;
re-declaring the same pair with a different metric is an error. The built index
is persisted beside the database in the `.hnsw` sidecar and reloaded on open.

A query consults the index through the `SEARCH … NEAR … TOPK` form; when no index
is declared, the engine falls back to an exact brute-force scan, which is the
oracle the index is validated against.

## Keyword (BM25) indexes

Lexical relevance complements semantic similarity. A keyword index is declared
on a `(label, property)` pair over a string property, and ranks documents by
Okapi BM25:

```console
$ drsg --db graph.drsg index keyword Doc body --plane social --lang english
```

Each index carries an analyzer **language** (`english`/`en`, `french`/`fr`, and
the other Snowball languages), which selects the stemmer and stopword set;
values are lowercased, tokenized, stopword-filtered, and stemmed before
indexing. The built index is persisted in the `.bm25` sidecar.

## Hybrid retrieval

Effective retrieval rarely depends on a single signal. Hybrid search combines up
to three channels and returns one ranked result:

- **Vector** — embedding similarity over a declared vector property,
- **Keyword** — BM25 relevance over a declared keyword property,
- **Graph** — proximity to the strongest vector/keyword hits, expanded outward
  and decayed per hop.

```console
$ drsg --db graph.drsg hybrid "how does time-travel work" \
    --plane social --label Doc \
    --vector embedding --keyword body \
    --graph-hops 1 --k 10 --embed openai
```

Each channel contributes a candidate pool; the scores are min-max normalized
within their channel and combined as a weighted sum (default weights: vector 1,
keyword 1, graph 0.5), and the top `k` are returned. Enabling a channel is a
matter of naming its property; the graph channel is enabled by `--graph-hops`.
The keyword channel requires a label, since a BM25 index is label-scoped.

## Natural-language querying

`plane.ask` compiles a plain-language question into a query, runs it, and returns
the connected subgraph. The model is grounded on the plane's soft schema (its
labels, property types, and edge-type connectivity) and instructed to emit a
serializable logical plan.

```console
$ drsg --db graph.drsg ask "which companies does Ada work for?" \
    --plane social --chat deepseek --embed qwen
```

The procedure is an agentic loop rather than a single call:

- **Decomposition.** A compound question is split into independent
  sub-questions, one plan per sub-question.
- **Grounded tools.** The model may call embedding-backed tools — `find_edge`
  and `find_entity` — that search the plane's own edge descriptors and entities
  by similarity, so it selects real edge types and seed nodes instead of
  guessing.
- **Repair.** A plan that fails to parse or execute is returned to the model with
  the error for a bounded number of repair attempts.

Querying is **read-only**: the logical plan has no write operators, so `ask`
cannot mutate the graph. `--dry-run` returns the generated plan without executing
it; the result also carries a trace of the tool calls and repairs for
inspection.

## Document ingestion (AIgest)

Ingestion turns unstructured text into graph structure. A document is chunked;
an LLM extracts entities and typed relations from each chunk; the extraction is
cleaned up (below); the entities are embedded; each is linked to an existing
node when one is found (avoiding duplicates) or created otherwise; and the
result is written through the bulk path.

```console
# Preview only (the default): report the entities and relations that would be written.
$ drsg --db graph.drsg digest notes.md --plane social --chat openai --embed openai

# Commit the extraction.
$ drsg --db graph.drsg digest notes.md --plane social --apply
```

### Extraction precision

Chunks are extracted independently, so nothing makes them converge: the same
kind of thing acquires several labels, the same relationship several edge types,
the same entity several names — and because chunks merge positionally, an
entity's properties are fixed by whichever chunk mentioned it *first*, with
every later and better mention discarded.

Three clean-up passes follow the extraction. They are not configured
individually; one option selects how many of them run, since they form a cost
ordering:

| `--mode` | passes | what it buys |
|---|---|---|
| `coarse` | vocabulary reconciliation | one label per kind, one edge type per relationship |
| `fine` *(default)* | \+ identity resolution | one node per entity |
| `super` | \+ per-entity refinement | properties drawn from the whole document |

- **Vocabulary reconciliation** canonicalizes the label set and the edge-type
  set — as *sets*, so it costs a constant number of model calls however long the
  document is. Names differing only in case or separators fold with no model
  involved.
- **Identity resolution** merges entities that name the same thing, rewrites
  edge endpoints onto the survivor, and collapses the duplicate edges and
  self-loops that produces. It also checks each key against the plane exactly,
  so re-digesting a document links rather than duplicating.
- **Per-entity refinement** re-reads each entity against *every* passage
  mentioning it, together with its relations, and asks for its properties again
  with the full picture in view. Entities with nothing to read beyond the chunks
  that produced them are skipped without a call. This is the pass that repairs
  first-chunk-wins — and it is expensive: **roughly 15× the input tokens** of
  `fine`, because each eligible entity carries its passages into a request of
  its own.

Wherever a name is canonicalized or merged, the form the document actually used
is kept beside it as `_label_as_written` / `_type_as_written` /
`_key_as_written`. Underscore-prefixed properties are provenance: they are
hidden from the schema summary the model reads, so an alias costs the read paths
nothing while keeping the document's own words recoverable.

The command-line tool ingests text and Markdown; the dashboard's **AIgest** page
additionally extracts PDF and DOCX, and previews the proposed entities and
relations before writing. Committing a preview performs no further model calls.

## Providers and keys

The LLM features — semantic search embedding, `ask`, and ingestion — call an
external provider. Presets are provided for **OpenAI**, **DeepSeek**, **Qwen**,
and **Ollama**; any other endpoint is addressed by base URL. The chat provider
and the embedding provider are chosen independently, which matters when a
provider offers one but not the other (DeepSeek, for instance, is chat-only —
pair it with Qwen for embeddings).

**Keys are read from the server's environment, never from a request.** Each
provider reads its conventional key variable (`OPENAI_API_KEY`,
`DEEPSEEK_API_KEY`, `DASHSCOPE_API_KEY`, …); these may also be supplied through
the `[llm]` section of the configuration file (see
[Chapter 2](./getting-started.md#configuration-file)).

## Primitives for agents

Two further capabilities, documented elsewhere, complete the agent-facing
surface and compose with the above:

- **Time-travel** — read any past state by commit sequence or timestamp, to
  audit or reproduce an earlier result. See [Chapter 4](./query-language.md).
- **Change feed** — subscribe to a plane and receive mutations as they commit,
  so an agent reacts to the graph instead of polling it. See
  [Chapter 6](./sdk.md).
