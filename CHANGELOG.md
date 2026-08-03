# Changelog

All notable changes to Dr Strange are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **URL ingestion — AIgest reads the web (ROADMAP §9).** A document may be named
  by address instead of uploaded. The server fetches the page, converts it to
  Markdown, follows its links under a budget, and assembles one document that
  digests exactly as a pasted one does.
  - `drsg digest <url>` (with `--topic`, `--pages`, `--depth`), a streaming
    `POST /digest/fetch`, and a URL row in the dashboard's AIgest view that
    returns a *list* of what was found — each page with its relevance score,
    ticked if it cleared the floor — so nothing becomes tokens unseen.
  - **Relevance is decided twice**, and hop count decides nothing. Anchor text,
    `title` and URL *path* words choose what is worth a request; the fetched
    text decides what is worth keeping. Both use the analyzer the BM25 index
    uses. A typed topic sharpens the page's own subject rather than replacing
    it. Hop decay survives only as a tiebreak toward the root.
  - The keep/drop floor is **relative to the best page in the batch**, because
    BM25 scores are not comparable across corpora and an absolute threshold
    would mean something different on every document.
  - Each page carries its address in the text as `<!-- drsg:source … -->`, and a
    page boundary now forces a chunk boundary, so no chunk mixes two documents.
  - Linked PDFs go through the existing extractor. Pages, depth, response size,
    total download and time are all bounded, and whatever a budget drops is
    reported rather than silently truncated.
- **`[fetch]` configuration section** — `enabled`, `max_pages`, `max_depth`,
  `concurrency`, `allow_private`. Fetching ships enabled; reaching the private
  network does not. The server refuses loopback, RFC-1918, link-local
  (`169.254.0.0/16`, where cloud metadata answers credentials) and other
  non-routable addresses, checks the **resolved address** rather than the
  hostname, and re-checks at every redirect hop. `allow_private` re-permits
  specific CIDR blocks for an operator who means it; it does not disable the
  guard. `robots.txt` is respected, the crawler identifies itself, and requests
  to one host are spaced.

### Known limitations
- A page whose text is assembled by JavaScript returns a shell with no prose;
  there is no headless renderer.
- Relevance is scored in one analyzer language per crawl.

## [1.3.0] - 2026-08-03

### Added
- **Extraction precision — AIgest in three passes (ROADMAP §8).** Digestion no
  longer stops at one round of per-chunk extraction. Three clean-up passes now
  follow it, exposed as a single `mode` on every surface rather than as five
  separate knobs:
  - `coarse` — **vocabulary reconciliation.** The label set and the edge-type
    set are canonicalized as *sets*, so the pass costs O(1) chat calls however
    long the document is. Names differing only in case or separators fold with
    no model involved; the model adjudicates the rest. Measured on one paper:
    70 labels → 39, 67 edge types → 32.
  - `fine` (the default) — **identity resolution.** Entities that name the same
    thing are merged (`Multi-Head Attention` / `Multi-head attention`, `K` /
    `Key`), edge endpoints are rewritten onto the survivor, and the duplicate
    triples and self-loops that creates are collapsed. Candidates come from
    cheap signals; only ambiguous pairs cost a call, and a pair whose entities
    carry different labels is never proposed.
  - `super` — **per-entity refinement.** Every entity mentioned outside the
    chunks that produced it is re-read against *all* of its passages plus its
    relations, repairing the properties that first-chunk-wins froze. Entities
    with nothing new to read are skipped without a call. Runs concurrently,
    merges in a deterministic order, and a failed refinement costs that entity
    rather than the run. Measured: 368 properties added and 109 revised across
    104 entities — and **~15× the input token usage**, stated in the CLI help,
    the OpenRPC summary, and an amber notice in the dashboard.
  - Reconciliation and merging keep the document's own wording beside the
    canonical form as `_label_as_written` / `_type_as_written` /
    `_key_as_written`, written only where the two differ.
- `--mode` on `drsg digest`, `mode` on `digest.run` (RPC + MCP, regenerated into
  all five SDKs), and a Mode select in the dashboard's AIgest view — remembered
  like the provider choices, with the cost of `super` shown where it is chosen.

### Fixed
- **A digested key could be written twice.** Duplicate prevention ran entirely
  through vector linking, so a plane whose nodes carry no usable embedding — as
  a digest without an embedder leaves them — matched nothing and created a
  second node under a key the plane already held, after which a key lookup
  silently answered with one of them. Exact-key matching against the plane no
  longer depends on embeddings.
- **A transient provider failure no longer discards a digest.** Requests are
  retried up to four times with exponential backoff (500 ms → 8 s), honouring
  `Retry-After`, but only for failures that might pass: transport errors, 429
  and 5xx. Every other 4xx is reported on the first try. Each provider
  round-trip is also bounded by a 300 s timeout, so a hung socket surfaces as an
  error instead of stalling the run indefinitely.

## [1.2.0] - 2026-08-02

### Added
- **Query-language parity (ROADMAP §7).** The openCypher subset now reaches
  every capability the engine has, so it is a complete alternative to
  hand-writing plan JSON:
  - `key(n)` reads a node's external key anywhere an expression is allowed; an
    equality or `IN` on the source variable compiles to a `SeekKeys` seek
    instead of a scan-and-filter.
  - `SEARCH (d:Doc) ON body MATCHING "…" [TOPK k]` — BM25 keyword search, the
    word-matching twin of the existing `NEAR` vector seed.
  - `HYBRID (d:Doc) [VECTOR …] [KEYWORD …] [GRAPH …] [CANDIDATES n] [TOPK k]` —
    fused retrieval with per-channel `WEIGHT`.
  - `CALL <pagerank|components|shortest_path|louvain>(args) ON (n[:Label])` —
    graph algorithms as a query source; the per-node result rides `score()`.
  - A trailing `AS OF <seq|"RFC-3339"|TIME ms>` clause pins any read to a past
    snapshot (native backend).
  - Every source may carry a relationship tail, so a typed hop follows a
    retrieval or algorithm seed; and `x IN [a, b]` works over any expression.
  - Only what a clause actually needs is required: `ON <property>` defaults to
    `embedding` wherever `NEAR` appears (`SEARCH`, `HYBRID VECTOR`, `BEAM`) and
    `HYBRID`'s `GRAPH DECAY` defaults to `0.5`, matching the RPC/MCP/CLI
    surfaces. `MATCHING` still requires `ON` — keyword properties follow no
    convention to default to, and it now says so.

### Documentation
- **Appendix B — Query-Language Grammar** (English and 中文): the complete
  grammar of the openCypher subset in one place, with every default, the
  algorithm signatures, and the constructs that are deliberately unsupported.
  The chapter explains what each construct is for; the appendix states exactly
  what parses. "LLM Included or Not" moves to Appendix C.

### Fixed
- A malformed clause reports its own position instead of blaming the query's
  first token: once a clause's leading keyword matches, the parser commits, so
  `HYBRID (n) VECTOR "model" …` points at `"model"` and a missing `RETURN`
  points at the end of the query.
- Matching plan sources — `Source::KeywordTopK`, `Source::Hybrid`,
  `Source::Algo` — and `Expr::ExternalKey`, so `plane.query` and the SDKs reach
  the same capabilities through serialized plans. `plane.hybrid()` and
  `Source::Hybrid` now share one fusion implementation.
- **One-line installers** for the released binaries — `scripts/install.sh`
  (Linux and macOS) and `scripts/install.ps1` (Windows). Each picks the archive
  for the host platform, verifies its published SHA-256, and installs `drsg`,
  `drsg-mcp`, or both onto the `PATH`; `--bin` / `--version` / `--dir` (and
  `DRSG_INSTALL_BIN` / `DRSG_VERSION` / `DRSG_INSTALL_DIR`) adjust the
  installation.
- Release binaries for **Intel macOS** (`x86_64-apple-darwin`), so the macOS
  installer covers both architectures.

### Changed
- **The embedded Rust API changed; every other surface did not.** The CLI,
  JSON-RPC, the five SDKs, MCP, plan JSON on the wire, and every query already
  written all keep working. For code embedding the Rust crates:
  `dr_strange_parser::parse` (and `parse_with_embedder`) now return a
  `ReadQuery { plan, as_of }` rather than a bare `LogicalPlan`, and
  `Statement::Read` carries it — `AS OF` addresses the plane handle, not the
  plan, so it cannot ride inside one. `Source` gained three variants and `Expr`
  one, which a total match must now handle.
- `Source`, `Step` and `Expr` are `#[non_exhaustive]`. A downstream match needs
  a wildcard arm from here on, so the operators and expression terms the engine
  keeps growing land as minor releases rather than major ones.

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

[Unreleased]: https://github.com/wangyingsm/dr-strange/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.2.0
[1.1.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.1.0
[1.0.2]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.0.2
