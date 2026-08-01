# Changelog

All notable changes to Dr Strange are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.1.0] - 2026-08-02

### Changed
- **LLM document ingestion is more robust and faster.** A chunk whose extraction
  reply hits the model's output-token cap now **auto-splits** and retries each
  half (recursing until it fits) instead of aborting the run, so large or dense
  documents self-heal. The per-chunk extraction calls also run **concurrently**
  through a bounded worker pool (configurable; `drsg digest --concurrency`,
  default 8), with entity-linking kept sequential and results merged in order so
  output stays deterministic.

### Added
- Server-side digest tuning. A `[digest]` config section (`concurrency`,
  `chunk_chars`) sets the defaults for `drsg serve`, and the `digest.run` RPC
  accepts per-request `concurrency` / `chunk_chars` overrides (typed in all
  SDKs). Precedence: request param → config → built-in (8 / 4000).

## [1.0.2] - 2026-08-01

Initial public release of **Dr Strange** — an AI-native, embedded graph database
written in Rust. Like SQLite, it links into an application and is backed by a
single on-disk database with no server to operate; unlike SQLite, `drsg serve`
also turns it into a standalone server with a JSON-RPC 2.0 API, a browser
dashboard, and a WebSocket change feed.

### Storage & versioning
- Hand-rolled log-structured-merge (LSM) storage engine with multi-version
  concurrency control (MVCC) — readers never block writers.
- A version-stamped cache and a single commit sequence that unify MVCC, caching,
  time-travel, and the change feed.
- **Time-travel** reads: query the graph as of any past commit or timestamp.
- Consistent, id-faithful **backup / restore** of the entire database.
- Optional legacy redb storage backend (`--no-default-features --features
  redb-backend`).

### Vectors & retrieval
- First-class vector embeddings as a native property type, indexed with HNSW.
- **Hybrid retrieval** fusing vector similarity, keyword (BM25), and
  graph-proximity into a single ranking.
- Graph algorithms: PageRank, connected components, shortest path, and Louvain
  community detection.

### AI-native features
- **Natural-language query**: an LLM turns a plain-language question into a
  read-only query plan grounded in the plane's schema, runs it, and repairs its
  own plan on error.
- **Live change feed**: subscribe to a plane and receive sanitized mutations
  over WebSocket as they commit.
- Document digestion and entity-resolution helpers.
- Model-backed features call an external or local LLM (OpenAI, DeepSeek, Qwen,
  or Ollama); provider keys come from the server environment, never request
  params. Every other feature runs with no model at all.

### Data model & query language
- **Planes**: many independent graphs within one database.
- A serializable logical query plan plus an openCypher-subset text language.

### Interfaces
- Embedded Rust library (`dr-strange-core`).
- `drsg` command-line tool: create planes, import and query data, serve,
  snapshot, and restore.
- Server mode: JSON-RPC 2.0 API, a Svelte web dashboard, and a WebSocket change
  feed.
- An **MCP server** (`drsg-mcp`) that exposes the database to LLM agents.
- Client **SDKs** for TypeScript, Python, Go, Java, and C, each with a live
  `watch()` change subscription.

### Distribution
- Prebuilt `drsg` and `drsg-mcp` binaries for Linux (`x86_64`, `aarch64`),
  macOS (Apple Silicon), and Windows (`x86_64`), each with a SHA-256 checksum.
- Multi-arch container image on GHCR — `ghcr.io/wangyingsm/dr-strange`
  (`linux/amd64` + `linux/arm64`).
- Documentation book (English and 中文) at
  <https://wangyingsm.github.io/dr-strange/>.
- Dual-licensed under MIT OR Apache-2.0.

[1.1.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.1.0
[1.0.2]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.0.2
