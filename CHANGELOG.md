# Changelog

All notable changes to Dr Strange are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.6.0] - 2026-08-10

### Added
- **`drsg serve` now hosts the MCP tool set at `POST /mcp`** (ROADMAP §10),
  so several agent hosts (e.g. Claude Code and Codex, each spawning its own
  MCP server subprocess) can share one database instead of each embedding
  its own — the other half of
  [#1](https://github.com/wangyingsm/dr-strange/issues/1), alongside the
  cross-process lock in 1.4.2. The endpoint runs the identical tool code
  `drsg-mcp` runs over stdio (extracted into a shared library so the two
  transports drive one implementation), over MCP's Streamable HTTP
  transport, gated by the same `DRSG_TOKEN` bearer auth as `/rpc` — with no
  token set, `/mcp` refuses every request, reads included, same as the rest
  of the authenticated surface. `drsg-mcp` itself is unchanged: it keeps its
  embedded stdio-only mode, since a host that wants the shared server can
  point its MCP client straight at the URL.
- **String predicates and membership in the query language.** `CONTAINS`,
  `STARTS WITH`, `ENDS WITH` — byte-wise like `=`, with non-string scalars
  promoted to text, so `d.year STARTS WITH "20"` matches whether `year` was
  stored as `2026` or `"2026"`. `IN` now also tests a value the row supplies:
  `"graph" IN d.tags` for a list property, or a map by key. It stays separate
  from `CONTAINS` because "list contains x" reads as either element-equality or
  substring, and openCypher splits them the same way.
- **A bound on how long a write waits for the writer slot.** `drsg serve` waits
  30s by default (`[server] write_timeout_secs`, `0` for forever) and answers
  JSON-RPC `-32002` — distinct from `-32000` because it is the one error worth
  retrying unchanged. Embedded callers are unaffected:
  `Database::set_write_timeout` is opt-in and still defaults to waiting.

### Changed
- **MCP tool calls are capped at 16 concurrent** (or `max_concurrent`, if
  lower); the excess queues. The request ceiling could not bound them — the
  transport answers a call as soon as it is *queued*, releasing its permit
  before the work starts.
- **MCP session idle window: 5 → 10 minutes.** The transport counts a *running*
  tool as idle, so a long `digest` on a quiet session was torn down mid-flight.

### Fixed
- **`drsg import` silently duplicated nodes whose external key already
  existed.** The bulk path rejects duplicates within a batch but never checked
  the plane, so a 2-node file imported twice left 4 nodes under 2 keys — the
  copy invisible to `key(n) = …`, with `drsg check` calling the database
  healthy. Import now refuses by default and names the keys; `--on-conflict
  skip` keeps the existing node, `update` overwrites its properties. This
  changes behaviour for anyone re-importing.
- **Ctrl-C could hang `drsg serve` while an agent host was attached.** The
  plain-HTTP listener drained without a deadline while the TLS path capped it at
  10s, and `/mcp`'s SSE stream never goes idle. Both now share one deadline.

## [1.5.0] - 2026-08-09

### Added
- **Chinese full-text search.** Chinese text was silently unsearchable: Han
  ideographs pass `char::is_alphanumeric`, so the split-based analyzer indexed
  whole clauses as single tokens and no sub-phrase query could ever hit.
  `Language::Chinese` segments with jieba in `cut_for_search` mode — the
  search-engine granularity where a compound also yields its sub-words, so a
  query for 数据库 still matches a document saying 图数据库. The embedded
  dictionary loads lazily behind a `OnceLock`, so databases that never index
  Chinese don't pay for it. The new language variant is appended last and its
  on-disk encoding is pinned by tests, so existing databases are unaffected.
- **Version policy with a migration ladder at open.** A database records the
  version that wrote it, and opening walks a ladder of migrations to bring an
  older one forward rather than failing or silently misreading it.

### Changed
- **Faster across the storage, vector, and graph paths.** Records and WAL
  batches are encoded by borrowing instead of cloning; SST blocks are scanned
  without allocating; node properties moved out of owned records, which also
  makes delete-node dedup cheaper; the catalog binary-searches its sorted
  connections list; hash collections standardized on ahash; and `bulk_load`
  was decomposed. Vector search gained multi-accumulator SIMD kernels for dot
  and L2 on x86-64 and aarch64, and the HNSW search beam got 2x headroom with
  hardened kernels and sidecar loads. The shipped binaries now use mimalloc.
  In the in-process benchmarks the graph query paths improved by more than an
  order of magnitude: 1-hop expansion ~270µs → 846ns, 2-hop ~364µs → 38µs, and
  HNSW top-k ~890µs → 13.3µs.

### Fixed
- **`Sort` did not define a total order over property values.** Keys were
  compared with `partial_cmp(..).unwrap_or(Equal)`, so a NaN compared equal to
  floats that still ordered among themselves — an inconsistent comparator that
  can produce an arbitrary permutation rather than merely misplacing the NaN.
  Sorting is now a genuine total order.
- **The on-disk micro-benchmarks reported the wrong backend.** They hard-coded
  the group label `"redb"` while `Database::open` selects its engine by cfg, so
  once `native-backend` became the default a plain `cargo bench` measured the
  native LSM engine and filed the result under `"redb"` — and redb stopped
  being measured at all. Because criterion keys its history by group name, this
  also made a post-change run compare native against a redb baseline and report
  the backend switch as an engine improvement. The label now derives from the
  same cfg. This affects benchmark reporting only, not shipped behavior.

## [1.4.2] - 2026-08-04

### Fixed
- **Two processes could open one database and silently destroy each other's
  writes** ([#1](https://github.com/wangyingsm/dr-strange/issues/1)). The native
  backend took no cross-process lock, so a second `Database::open` on the same
  directory succeeded and got its own WAL offset and `next_sst` counter.
  Measured: two concurrent 200-node imports left a database holding 200 nodes,
  with `drsg check` reporting it healthy — silent loss that survives the
  integrity scan. The engine now takes an exclusive advisory lock on
  `<dir>/LOCK` for its lifetime, so the second open fails with an error naming
  the database and pointing at `drsg serve`. Closing releases it, so
  close-then-reopen is unaffected. A filesystem that cannot lock (some network
  mounts) is warned about and allowed through rather than made unusable. The
  `redb` backend was never affected — redb locks internally.

  This matters most for MCP: every agent host spawns its own `drsg-mcp`
  subprocess, so two editors on one project were two writers on one database.

## [1.4.1] - 2026-08-04

### Added
- **A legible Explore canvas.** The plot drew 200 arbitrary nodes and no layout
  makes 200 interlinked nodes readable, so it now draws the skeleton and lets
  the reader ask for more.
  - `graph.seed` gains `order` (`scan` \| `degree` \| `pagerank`) and returns
    the scores it ranked by. Explore opens on the 40 most connected nodes with
    a *show more*. Prefer `degree`: PageRank pools rank in sinks, so a hub can
    score below its own neighbours.
  - The legend is a filter — click a label to hide that category.
  - A hub's leaves fold into one counted bead past twenty and open on a click;
    below that they are arranged into arcs by label.
  - Edges touching a well-connected node lay out longer, so a busy
    neighbourhood has room.
  - A selection fades the graph by hop distance, and selecting an **edge**
    focuses both of its endpoints.

### Fixed
- **The dashboard could serve a previous build.** `index.html` went out with no
  `Cache-Control`, `ETag` or `Last-Modified` — and it is the only unhashed file
  and the only thing naming the hashed bundles — so a browser could reuse a
  stale copy pointing at assets the rebuild had replaced. The SPA fallback then
  answered the missing bundle with `index.html`, handing back HTML where the
  browser asked for JavaScript, so the page failed silently. The entry point is
  now `no-cache`, hashed assets are `immutable`, and a missing asset is a 404.

## [1.4.0] - 2026-08-03

### Added
- **URL ingestion (ROADMAP §9).** A document may be named by address instead of
  uploaded: the server fetches the page, converts it to Markdown, follows its
  links under a budget, and assembles one document that digests as a pasted one
  does. `drsg digest <url>` (`--topic` / `--pages` / `--depth`), a streaming
  `POST /digest/fetch`, and a source row in the dashboard that returns the pages
  it found — each scored, most relevant first — to tick before they cost tokens.
  - **Relevance is decided twice**: anchor text, `title` and URL *path* words
    choose what is worth a request; the fetched text decides what is worth
    keeping. Hop count is only a tiebreak. The keep floor is relative to the
    best page in the batch, since BM25 scores are not comparable across corpora.
  - Each page carries `<!-- drsg:source … -->`, and a page boundary now forces a
    chunk boundary, so no chunk mixes two documents.
  - Linked PDFs reuse the existing extractor. Pages, depth, response size, total
    download and time are bounded, and what a budget drops is reported.
- **`[fetch]` configuration** — `enabled`, `max_pages`, `max_depth`,
  `concurrency`, `allow_private`. Fetching ships enabled; reaching the private
  network does not. Loopback, RFC-1918, link-local (`169.254.0.0/16`, where
  cloud metadata answers credentials) and other non-routable addresses are
  refused on the **resolved address**, re-checked at every redirect hop.
  `allow_private` re-permits specific CIDR blocks; it does not disable the
  guard. `robots.txt` is respected and per-host requests are spaced.

### Known limitations
- JavaScript-rendered pages return a shell with no prose; no headless renderer.
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

[Unreleased]: https://github.com/wangyingsm/dr-strange/compare/v1.6.0...HEAD
[1.6.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.6.0
[1.5.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.5.0
[1.2.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.2.0
[1.1.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.1.0
[1.0.2]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.0.2
