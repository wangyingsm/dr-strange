# Getting Started

This chapter takes you from an empty directory to a running graph you can query
from the command line and explore in the browser.

## Prerequisites

- A recent **Rust toolchain** (stable), via [rustup](https://rustup.rs).
- Optional, for the web dashboard: **[bun](https://bun.sh)** (to build the
  single-page app) and **[just](https://github.com/casey/just)** (task runner).

## Building

Dr Strange is a Cargo workspace. Build the command-line tool, `drsg`:

```console
$ cargo build --release -p dr-strange-cli
```

That produces `target/release/drsg`. The default build uses the native LSM
storage engine; a legacy redb backend is available behind a feature flag if you
ever need it.

To ship the **real dashboard** (rather than a placeholder page) inside the
binary, build the web SPA first — it is embedded at compile time:

```console
$ just web-build          # bun install + vite build
$ cargo build --release -p dr-strange-cli
```

## The database on disk

Point `--db` at a path. With the native backend the database is a **directory**
(the write-ahead log and sorted SST files live inside it), with two sidecar
files beside it for the search indexes:

```text
graph.drsg/          ← the database (WAL + SST files)
graph.drsg.hnsw      ← vector-index sidecar
graph.drsg.bm25      ← keyword-index sidecar
```

The database is created on first use; there is no separate "init" step.

## Your first graph

Create a plane, then add some data. You can write it in the openCypher subset:

```console
$ drsg --db graph.drsg plane create social

$ drsg --db graph.drsg cypher --plane social \
    'CREATE (a:Person {name:"Ada"}),
            (b:Person {name:"Alan"}),
            (a)-[:KNOWS]->(b)'
```

Read it back:

```console
$ drsg --db graph.drsg cypher --plane social \
    'MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN q'
```

Check the shape of what you have:

```console
$ drsg --db graph.drsg stats
$ drsg --db graph.drsg catalog --plane social
```

## Adding an embedding and searching by meaning

Vectors are ordinary properties. Declare an index on a `(label, property)` pair,
then search it. Embedding a *text* query happens server-side, so the process
needs a provider key in its environment (e.g. `OPENAI_API_KEY`); alternatively
you can search with a literal vector and no provider.

```console
$ drsg --db graph.drsg index ensure Doc embedding --plane social

$ OPENAI_API_KEY=… drsg --db graph.drsg cypher --plane social \
    'SEARCH (d:Doc) ON embedding NEAR "a friendly greeting" TOPK 5 RETURN d'
```

Because the results are graph nodes, you can traverse onward from them — this is
the GraphRAG pattern from Chapter 1.

## Serving the dashboard

```console
$ drsg --db graph.drsg serve
```

This starts the JSON-RPC API, the WebSocket change feed, and the embedded
dashboard, and prints the address (default `http://127.0.0.1:7700`). Open it to
explore the graph, ingest documents, run queries, and watch changes live.

**Authentication.** With no token set, only the same-origin browser UI is
allowed to call the API. To permit programmatic access (SDKs, `curl`), set a
shared token before serving and present it as a bearer token:

```console
$ DRSG_TOKEN=please-change-me drsg --db graph.drsg serve
```

## Where to go next

- **Chapter 3 — AI Native:** embeddings, hybrid retrieval, natural-language
  querying, and document ingest.
- **Chapter 4 — Query Language:** the full openCypher subset and the logical
  plan beneath it.
- Prefer code? Jump to **Chapter 6 — SDK**. Prefer the shell? **Chapter 7 —
  Embedded CLI**. Building an agent? **Chapter 8 — MCP**.
