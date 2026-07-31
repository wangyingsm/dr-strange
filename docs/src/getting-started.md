# Getting Started

This chapter takes you from an empty directory to a running graph you can query
and explore in the browser.

## Building

Dr Strange is a Rust workspace. Build the command-line tool (`drsg`) with Cargo:

```console
$ cargo build --release -p dr-strange-cli
```

This produces `target/release/drsg`. The default build uses the native LSM
storage backend; the legacy redb backend is available behind a feature flag.

## Your first graph

```console
# Create a plane and add a couple of nodes and an edge.
$ drsg --db graph.drsg plane create social
$ drsg --db graph.drsg cypher --plane social \
    'CREATE (a:Person {name:"Ada"}), (b:Person {name:"Alan"}), (a)-[:KNOWS]->(b)'

# Read it back.
$ drsg --db graph.drsg cypher --plane social \
    'MATCH (p:Person)-[:KNOWS]->(q) RETURN p, q'
```

## Serving the dashboard

```console
$ drsg --db graph.drsg serve
```

Then open the printed address (default `http://127.0.0.1:7700`) to explore the
graph, ingest documents, run queries, and watch changes live.

## Sections (draft)

- Prerequisites (Rust toolchain; optional: `just`, `bun` for the web UI)
- Building from source (CLI, and the embedded web dashboard)
- The database file / directory layout
- A first plane, nodes, and edges (CLI and Cypher)
- Adding an embedding and running a similarity search
- Starting `drsg serve` and the auth token (`DRSG_TOKEN`)
- Where to go next (per interface: SDK, CLI, MCP)
