<p align="center">
  <img src="crates/dr-strange-web/frontend/public/magic-circle.svg" alt="Dr Strange" width="120" height="120">
</p>

<h1 align="center">Dr Strange</h1>

<p align="center"><em>An AI-native embedded graph database, written in Rust.</em></p>

<p align="center">
  <a href="https://github.com/wangyingsm/dr-strange/actions/workflows/ci.yml"><img src="https://github.com/wangyingsm/dr-strange/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/wangyingsm/dr-strange/actions/workflows/release.yml"><img src="https://github.com/wangyingsm/dr-strange/actions/workflows/release.yml/badge.svg" alt="Release"></a>
  <a href="https://github.com/wangyingsm/dr-strange/actions/workflows/docs.yml"><img src="https://github.com/wangyingsm/dr-strange/actions/workflows/docs.yml/badge.svg" alt="Docs"></a>
  <a href="https://github.com/wangyingsm/dr-strange/releases/latest"><img src="https://img.shields.io/github/v/release/wangyingsm/dr-strange?label=release&color=blue" alt="Latest release"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License: MIT OR Apache-2.0"></a>
</p>

<p align="center"><strong>English</strong> · <a href="README.zh.md">简体中文</a></p>

📖 **The Dr Strange Book** — the full tutorial and guide:
[English](https://wangyingsm.github.io/dr-strange/en/book/introduction.html) ·
[中文](https://wangyingsm.github.io/dr-strange/zh/book/introduction.html).

## Introduction

Dr Strange is a graph database designed from the outset for AI workloads:
embeddings are a first-class value type, similarity search operates alongside
graph traversal, and the engine exposes primitives suited to agents — natural
language queries, a live change feed, and time-travel — rather than adding AI
features to a conventional graph database after the fact.

Like SQLite, it is **embedded**: a library linked into an application, backed by a
single on-disk database, with no server to operate. Unlike SQLite, it can also
**serve** — `drsg serve` exposes a JSON-RPC 2.0 API, a browser dashboard, and a
WebSocket change feed, with client SDKs in five languages.

For applications built around a knowledge graph, a GraphRAG pipeline, or an
agent's long-term memory, Dr Strange aims to be the single store for all of it.

## Web UI screenshots

<table>
  <tr>
    <td width="50%"><a href="screenshots/00.jpg"><img src="screenshots/00.jpg" width="100%" alt="Dashboard — plane statistics and management"></a><br><sub><b>Dashboard</b> — live plane statistics and management</sub></td>
    <td width="50%"><a href="screenshots/01.jpg"><img src="screenshots/01.jpg" width="100%" alt="Explore — interactive graph with a node inspector"></a><br><sub><b>Explore</b> — interactive graph with a node inspector</sub></td>
  </tr>
  <tr>
    <td width="50%"><a href="screenshots/02.jpg"><img src="screenshots/02.jpg" width="100%" alt="Algorithms — shortest path on the graph"></a><br><sub><b>Algorithms</b> — PageRank, communities, and shortest path</sub></td>
    <td width="50%"><a href="screenshots/03.jpg"><img src="screenshots/03.jpg" width="100%" alt="AIgest — LLM document ingestion into entities and relations"></a><br><sub><b>AIgest</b> — LLM document ingestion into entities &amp; relations</sub></td>
  </tr>
</table>

## Features

| Capability | What it gives you |
|---|---|
| **Planes** | many independent graphs in one database |
| **First-class embeddings** | vector properties, natively HNSW-indexed |
| **Hybrid retrieval** | fused vector + keyword (BM25) + graph-proximity search |
| **Query language** | a serializable logical plan and an openCypher subset |
| **Graph algorithms** | PageRank, connected components, shortest path, Louvain |
| **Natural-language query** | ask in plain language → plan → run |
| **Time-travel** | read the graph *as of* a past commit or timestamp |
| **Change feed** | subscribe to a plane and receive mutations live |
| **Backup / restore** | consistent, id-faithful whole-database snapshots |
| **Interfaces** | a web UI, five language SDKs, a CLI, and an MCP server |

The model-backed features (natural-language query, document ingestion, and
text-embedding search) call an external or local LLM; everything else runs with
no model at all. See [Appendix C](https://wangyingsm.github.io/dr-strange/en/book/appendix-c.html).

## Install

One line, no toolchain. The installer downloads the released binary for your
platform, verifies its SHA-256, and puts it on your `PATH`. Two binaries are
available: the CLI and server, `drsg`, and the MCP server for LLM agents,
`drsg-mcp`.

**Linux**

```console
# CLI and server — drsg
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh

# MCP server — drsg-mcp
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin drsg-mcp
```

**macOS** (the same script; Apple silicon and Intel)

```console
# CLI and server — drsg
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh

# MCP server — drsg-mcp
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin drsg-mcp
```

**Windows** (PowerShell)

```console
# CLI and server — drsg
PS> irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1 | iex

# MCP server — drsg-mcp (run as a block: a piped script cannot take arguments)
PS> & ([scriptblock]::Create((irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1))) -Bin drsg-mcp
```

`--bin all` installs both binaries; `--version v1.1.0` pins a release, and
`--dir <path>` chooses the destination (default `~/.local/bin`, or
`%LOCALAPPDATA%\Programs\drsg\bin` on Windows). On Windows the flags are
`-Bin`, `-Version`, and `-Dir`.

Alternatives: the container image, `ghcr.io/wangyingsm/dr-strange:latest`, or the
archives and checksums on the
[releases page](https://github.com/wangyingsm/dr-strange/releases).

**From source** — a last resort, for platforms with no published binary or to
build a working copy. Requires a [Rust toolchain](https://rustup.rs); the
dashboard is embedded at compile time, so build the SPA first (`just web-build`,
which needs [bun](https://bun.sh)) or the binary ships a placeholder page.

```console
$ cargo build --release -p dr-strange-cli   # → target/release/drsg
$ cargo build --release -p dr-strange-mcp   # → target/release/drsg-mcp
```

## Getting Started

```console
# Create a plane, add data, and query it.
$ drsg --db graph.drsg plane create social
$ drsg --db graph.drsg cypher --plane social \
    'CREATE (a:Person {name:"Ada"})-[:KNOWS]->(b:Person {name:"Alan"})'

# Serve the dashboard + API.
$ drsg --db graph.drsg serve
```

The full walkthrough — building, the on-disk layout, embeddings and similarity
search, the server and its configuration, and the container image — is in the
book's **Getting Started** chapter:
[English](https://wangyingsm.github.io/dr-strange/en/book/getting-started.html) ·
[中文](https://wangyingsm.github.io/dr-strange/zh/book/getting-started.html).

## Documentation

The book covers each part in depth:
[AI Native](https://wangyingsm.github.io/dr-strange/en/book/ai-native.html) ·
[Query Language](https://wangyingsm.github.io/dr-strange/en/book/query-language.html) ·
[Web UI](https://wangyingsm.github.io/dr-strange/en/book/web-ui.html) ·
[SDK](https://wangyingsm.github.io/dr-strange/en/book/sdk.html) ·
[Embedded CLI](https://wangyingsm.github.io/dr-strange/en/book/embedded-cli.html) ·
[MCP](https://wangyingsm.github.io/dr-strange/en/book/mcp.html) ·
[JSON-RPC API list](https://wangyingsm.github.io/dr-strange/en/book/appendix-a.html) ·
[Query-language grammar](https://wangyingsm.github.io/dr-strange/en/book/appendix-b.html).

Build it locally (mdBook): `just docs-serve` (English) or `just docs-serve zh`.

## Architecture

Dr Strange is built in distinct layers — storage (a hand-rolled LSM engine with
MVCC), a version-stamped cache, computation, the API surface, and the
cross-cutting plane model — with the wrapper layers (web, SDKs, CLI, MCP, LLM)
above the core.

- **[Architecture chapter](https://wangyingsm.github.io/dr-strange/en/book/architecture.html)** — the layer map and how
  the commit sequence unifies MVCC, caching, time-travel, and the change feed.
- **[`arch/`](arch/)** — the detailed, per-layer design notes:
  [overview](arch/00-overview.md),
  [storage](arch/01-storage.md),
  [cache](arch/02-cache.md),
  [computation](arch/03-computation.md),
  [API](arch/04-api.md),
  [planes](arch/09-planes.md).

## Benchmarks

A cross-engine comparison against an embedded graph DB (Kùzu), the universal
embedded baseline (SQLite), and the industry-standard server (Neo4j). Every
engine loads the **same** deterministic dataset — 100 K nodes, 500 K edges,
128-dim vectors — and runs the **same** query sets on its own optimal path.

| Operation (median latency, ↓ better) | dr-strange | Kùzu | SQLite | Neo4j |
|---|---|---|---|---|
| Point lookup by key | 12.0 µs | 306.8 µs | **3.8 µs** | 783.0 µs |
| 1-hop expansion | 17.7 µs | 2.03 ms | **9.0 µs** | 504.5 µs |
| 2-hop reachable set | **53.2 µs** | 8.94 ms | 69.5 µs | 1.08 ms |
| Vector top-k query | **320.5 µs** | 9.28 ms | — | 3.51 ms |

The embedded KV design keeps point and graph queries in microseconds — fastest
of the field on multi-hop traversal — and vector search is where it pulls away:
top-k an order of magnitude below Neo4j and ~30× below Kùzu, with index build
several times faster than both (full table in BENCHMARKS.md). Bulk load still
trails the mature columnar engines. Numbers are single-run, warm, on one
machine, all engines measured back-to-back — **indicative, not a leaderboard**.
Methodology, caveats, the load-throughput figures, and how to re-run
(`just bench-compare`, or `just benchmark` for dr-strange alone) are in
**[BENCHMARKS.md](BENCHMARKS.md)**.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
