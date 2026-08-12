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
additionally converts Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV
and PDF to Markdown, and previews the proposed entities and
relations before writing. Committing a preview performs no further model calls.

### Reading from a URL

A document may be named by address instead of uploaded. The server fetches the
page, converts it to Markdown, follows its links under a budget, and assembles
one document — which then digests exactly as a pasted one does.

```console
$ drsg --db graph.drsg digest https://example.com/paper \
    --plane papers --topic "attention mechanism" --pages 6 --apply
```

A page's outbound links are a curated bibliography: the author already decided
what is related. The difficulty is that they also point at cookie policies and
navigation. So **relevance is decided twice**, and hop count decides nothing —
depth measures how far the crawl walked, not whether a page is about anything.

- *Before fetching*, each link is scored on what is already in hand: its anchor
  text, its `title`, and the words in its URL path. No network, no model. Only
  the best candidates cost a request.
- *After fetching*, the page is scored again on its actual text, and one that
  does not hold up is dropped having cost exactly one request.

Both use the same analyzer as the BM25 index, so a link saying "transformers"
matches a target term of "Transformer". A typed `--topic` sharpens the target;
left empty, the page's own subject is what the crawl looks for. Hop decay
survives only as a tiebreak toward the root.

Each page arrives with its address recorded in the text, and a page boundary
always begins a new chunk, so no chunk ever mixes two documents:

```markdown
<!-- drsg:source https://example.com/paper -->

# Attention Is All You Need
…
```

Budgets — pages, depth, response size, total download, time — are the real
control, and **whatever a budget drops is reported**. The CLI prints what it
kept and what it discarded; the dashboard shows the same list with checkboxes.

Fetching is enabled by default. What is not a default is reaching the private
network: the server refuses to connect to loopback, private, link-local (where
cloud metadata services live) or otherwise non-routable addresses, checks the
*resolved address* rather than the hostname, and re-checks it at every redirect
hop. An operator who needs an intranet source re-permits exactly that block, and
this is the one exception; see [Chapter 2](./getting-started.md#configuration-file).
`robots.txt` is respected, the crawler identifies itself, and requests to one
host are spaced.

Known limitation: a page whose text is assembled by JavaScript in the browser
returns a shell with no prose. There is no headless renderer.

### Reading a codebase

Point `digest` at a directory and it walks the tree, routing each file to
whatever handles it. Source files are **parsed** rather than read: the result is
facts a compiler-grade parser is certain of, not a model's reading of the text.

```console
$ drsg --db graph.drsg digest ./crates/dr-strange-core/src --plane code --apply
preprocessed by rust@1 (3878 facts)
no prose left to read — digested without a model call
  0 chat request(s); tokens 0 in / 0 out / 0 embed
applied: wrote 1139 nodes, 2739 edges
```

Read that third line again: **no API key was set and no request was made.** An
AST does not infer that `parse()` calls `lex()` — it knows — so handing the file
to a model as prose would spend tokens to get a worse answer than the one
already in hand. Where a parser is certain, the model is not consulted.

Each handler brings a vocabulary of its own, fixed by the handler rather than
invented per document — which is why nothing needs reconciling afterwards, and
why **the labels and properties a handler emits are documented by that handler**,
not here. Ask the one you are using; `drsg digest --handler <name>` names it.

Every fact carries `_generated_by` naming the handler and its version, so a
parsed fact is always distinguishable from a model's guess. Where both claim one
key, **the parser wins** and the model's version is dropped and counted.

The example above points at `src/` rather than the crate root, and that is the
difference between a run that calls no model and one that does: a crate root
holds a `Cargo.toml` and usually a README, and those are prose that genuinely
needs reading. Pointed at a `src/` directory, the crate is still named after the
directory holding it, so two crates ingested into one plane stay two crates.

A real repository is not one language, so a tree is routed per file: the Rust is
parsed, the Markdown and the configuration become prose for the model, and
anything unreadable is skipped and *counted*. What the parser could not resolve
is reported rather than quietly omitted — a call into another crate is not an
edge, and a name matching two functions equally is left alone rather than
guessed at:

```
note: 553 call(s) named nothing defined here — calls into other crates and
      the standard library are not edges
```

Two flags: `--handler rust` forces a handler instead of routing by extension,
and `--plugin-source` stores each function's own source on its node for
retrieval. The second is off by default — it is roughly a copy of the codebase
in the graph, and properties share one record, so every read of that node would
decode the body too.

The walk honours `.gitignore` and `.dockerignore` (a project's own statement of
what is derived rather than source, and better than a list this tool could
guess at) and always skips `target/`, `node_modules/` and their kin. Running it
twice on an unchanged tree yields the same graph, byte for byte.

This is deliberately **local-only** — `drsg` on your own machine and the stdio
MCP server, never a shared `drsg serve`. What makes parsing worth its cost is a
handler pulling the files *around* the one it was handed, and the only
filesystem a shared server could offer is its own.

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
