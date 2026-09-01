# Changelog

All notable changes to Dr Strange are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- **`--no-default-features` builds the CLI again.** It had not compiled for
  several releases: the `digest` feature gates `DigestArgs`, `commands::digest`
  and `config::plugin_config`, but not the `Command::Digest` match arm that
  calls them, nor two plugin-store helpers, nor a `PropDesc`/`PropValue` import
  only gated code used. Gated now — and `[plugins]` in `drsg.toml` still
  *parses* in a build with no plugin host, because one config file is shared by
  every binary an operator runs and rejecting a section this build merely has
  no use for would make that file un-shareable.

### Added
- **A `features` CI job, and `just gate-features` beside it.** The `rust` job
  builds exactly one configuration — all defaults — so everything this project
  documents as optional was unbuilt by CI, and a missing `#[cfg]` stayed
  invisible until someone tried the combination. That is precisely how the
  breakage above survived several releases with nothing going red. The new job
  builds the CLI without the digest pipeline on both backends, the LLM crate
  without the wasm plugin host, and the MCP and web crates on the legacy
  backend, `--all-targets` throughout: a test helper used only by gated tests
  is dead code in the build without them, which is one of the errors this
  found.

### Changed
- **The official plugin catalog is data, fetched, not a constant of the
  binary.** It lived as a nine-entry `const` in `dr-strange-llm`: a release
  URL and a pinned hash per plugin. That made every plugin release a change to
  *this* repository — tag `rust-v1.4.2` in the extensions repo, then edit a
  Rust file here, bump, ship — for a fact the host merely repeats, and the two
  projects release apart on purpose. The list now lives beside the plugins as
  [`catalog.json`](https://github.com/wangyingsm/dr-strange-extension/blob/master/catalog.json)
  in the extensions repository, where the release workflow writes each new
  version, URL and hash itself. A plugin release reaches every installed drsg
  without a drsg release.

  Moving the list out did not move the judgement out. Each entry now carries
  `contract` (the WIT world it was built against, weighed against the one this
  host speaks) and `min_drsg` (the oldest host it claims to work with), and its
  `sha256` is verified on download before the bytes are looked at as a
  component — the store's own pin, re-checked at every load, is unchanged.
  Several entries may share a name, so a plugin can keep serving older hosts:
  each host installs the newest entry it can run. An entry this build cannot
  run is **listed with the reason, not hidden** — the wasm loader is the real
  gate, and a plugin silently absent from the list is a support question.

- **`plugin.catalog` returns `{stale, schema, source, plugins}`** instead of a
  bare array, and each entry gained `version` and `compat`. The server caches
  the catalog for an hour and falls back to the store's copy when the fetch
  fails; `stale: true` says so, and the dashboard's Extensions panel prints a
  line when it is showing an older copy. Regenerated in every SDK.

### Added
- **`drsg update`.** Asks GitHub for the newest release — through the
  `releases/latest` redirect rather than the rate-limited API, as
  `scripts/install.sh` does — and, when this build is behind it, `exec`s the
  same `curl … | sh` a first install runs. It hands over rather than
  reimplementing the download: the installer already gets the target triple,
  the checksum and the atomic replace right, and a second installer would only
  ever be exercised by people upgrading. `exec` rather than spawn because the
  file this process was loaded from is about to be overwritten, and a parent
  waiting to print "done" would be waiting inside it.

  The installer is pointed at the directory the running binary is in, not its
  own `~/.local/bin` default — an upgrade has to replace the copy on the
  `PATH`, not add a newer one elsewhere and leave the old one being run.
  `--dir` overrides it, `--bin all` takes `drsg-mcp` along. A build *newer*
  than the latest release is told it is ahead and nothing is installed, so
  `update` never moves anyone backwards; on Windows nothing runs at all,
  because the executable is locked while running, and the command to paste is
  printed instead.

- **`drsg plugin install <name>`.** A bare word is now looked up in the
  official catalog — `drsg plugin install rust` — rather than read as a
  filename. Paths and URLs are unchanged, and the interactive chooser takes a
  name as well as a number.
- **`drsg plugin list --available`.** The catalog as a table, each entry tagged
  `[installed]`/`[upgradable]` against the local store, with `--json` for
  agents. The same information the interactive installer shows, without the
  prompt.
- **The catalog is cached beside the installed plugins**, so an offline
  `drsg plugin install` still lists it and says how old the copy is. With no
  cache and no network it fails naming the URL and the way around it — a path
  or a URL needs no catalog. Nothing is vendored into the binary: a snapshot in
  this tree is the thing being removed, and one that went stale silently would
  be worse than an error that says so.

## [2.2.1] - 2026-08-28

### Changed
- **The pinned `go` plugin moves to `go-v1.4.0`.** A generated `.pb.go` used
  to refuse the whole repository it sat in: protoc-gen-go writes the
  descriptor blob as one string concatenation — 955 terms in the tree this
  came from — and the plugin rendered that back out of the AST to record it,
  which recursed once per term and walked the guest's 64 KiB stack past zero.
  It surfaced as an out-of-bounds memory access that never mentioned a stack.
  The plugin now takes an initializer from the file by offset instead of
  printing it, and caps it, so `value` is a bounded prefix rather than a
  hundred kilobytes of escaped bytes; it is also linked with a 1 MiB stack,
  which is what `go/parser` needs for deeply nested literals — that recursion
  runs before the plugin's own code sees anything. Pins move with the host, so
  the new artifact and its SHA-256 land here.

### Fixed
- **A `RETURN` this subset does not have now says so.** Appendix B promises
  projections and aggregation are "a clear error, never a silent
  mis-compile", but `RETURN f.file, f.line, key(f)` stopped the parse at the
  first dot and surfaced as `unexpected trailing input near \`.file, …\`` — a
  position, not an answer, and it reads like a typo in a query that has none.
  Each shape now names itself and says what to write instead: a projection, a
  column list, a call in RETURN, an `AS` alias. Written for the callers who
  actually hit it — agents on the MCP `cypher` tool, for whom the error
  message is the only documentation in reach. The tool's own description now
  states the restriction too, so the query arrives right the first time.
- **The MCP `search` tool pointed at a config section that does not exist.**
  Without an embedding provider it said to set "`[server]` embed provider",
  but the keys live under `[digest]` — and `[server]` denies unknown fields,
  so an operator who followed the hint got a server that would not start. It
  now names `[digest] embed_provider` and `embed_key_env`, and points at
  `grep` for the text search that needs no provider at all.
- **One file a plugin cannot parse no longer refuses the whole repository.**
  A chunk is one file, so a `parse` that traps is now counted and skipped —
  named in the report, logged at `warn` with its backtrace — the way the
  built-in reader has always counted a PNG it cannot convert. Found on a
  real tree: the `go` plugin walks its printer down a `.pb.go` whose
  generated `rawDesc` is a thousand-term string concatenation, overflows the
  stack TinyGo linked it with, and traps. That one file refused the entire
  digest, and under `serve watch` it refused every fold after it too — the
  watcher stopped on the first rebuild and the plane stayed empty while the
  server went on answering queries against it. Failing on *every* file is
  still fatal: that is a plugin that does not work here, not a difficult
  file, and quietly ingesting nothing would be worse than saying so.
- **A trap now says what the trap was.** wasmtime puts the wasm backtrace in
  the outer message and the code that names the fault in the cause, and the
  host rendered only the outer one — so the log got twenty frames of
  recursion and never the words "out of bounds memory access". The code now
  leads the message, and a trap that reads as a guest running off its own
  stack (a wrapped stack pointer, or one frame repeated all the way down)
  says so, along with the fact that no host setting can raise a stack that
  was fixed when the plugin was linked.

## [2.2.0] - 2026-08-26

### Added
- **`drsg digest` now reads a repository's history, into a plane of its own.**
  A checkout carries two sources of truth: the tree says what the code is, and
  the repository says how it got there. Digesting a directory that turns out
  to be a git checkout now also reads its **commits, branches, tags, merges
  and rebases** into `<plane>_git`, beside the code plane — facts only, and
  never a model call. The reading is done by a new sandboxed plugin, `git@1`,
  in the extensions repository: it carries its own reader for git's object
  store (loose objects, v2 pack indexes, both delta forms), refs and reflog,
  so **no `git` binary is run** and none is required.

  Two planes rather than one, because the two answer different questions and
  have different lifetimes: a code plane is a picture of the tree *now* and is
  rewritten whenever a file changes, while history only ever grows. Writing is
  append-mostly and shaped by what can actually change — a commit is immutable,
  so one already in the plane is left alone and its `PARENT` edges are never
  rewritten; only a moving pointer (a branch, a tag, a rebase) is patched. A
  second digest of an unchanged repository writes nothing at all.

  `--no-git` turns the stage off; `--git-plane <name>` puts it somewhere else.
  The history stage runs **before** anything that can reach for a model, so a
  digest that dies on a missing API key does not take the repository's history
  down with it. `[plugins.git]` carries the settings (`max_commits`, `reflog`,
  `remotes`, `tags`, `body`).

  What a `Rebase` node can and cannot claim is stated rather than implied: a
  rebase leaves no trace in the commit graph — it writes new commits and moves
  a ref — so the only record is the reflog, which is local to one clone and
  expires (`gc.reflogExpire`, 90 days by default). Rebases are reconstructed
  from it, the report says so, and an absent `Rebase` never means "no rebase
  happened". The same reflog is why commits no ref can still reach are kept
  and marked `reachable: false`: they are what a rewrite left behind.

- **`drsg history`, and an MCP tool of the same name.** One verb that orients a
  reader in a repository: where HEAD is, what the branches and tags point at,
  which branches were rebased and what each replaced, and the newest commits —
  as compact text, the way `context` answers "what is this symbol". Naming the
  code plane finds the history beside it (`myrepo` → `myrepo_git`), because the
  first is what a reader has in mind. Every listing says what it is a listing
  *of* (`newest 15 of 429`): a truncated one that looked complete is the one
  failure a reader cannot see.

- **`serve watch` keeps the history plane current, so `drsg init` bootstraps
  both.** History is read at startup and again on every HEAD move, beside the
  code fold and sharing the plugins it already loaded. A commit that touched no
  file the code plane holds — an empty one, or one that moved only something
  ignored — still lands, because it moved a branch. `--no-git` turns it off, as
  on `digest`. A tag or branch created *without* a commit reaches the plane on
  the next HEAD move: the watcher wakes on HEAD, and polling every ref would
  double its git calls for a rare case.

- **The agent surface says the history plane exists.** `list_planes` now labels
  each plane with what it holds and names its counterpart, and the MCP server's
  instructions carry the history vocabulary (`Commit`/`Merge`, `Branch`, `Tag`,
  `Rebase`; `PARENT` with `order`, `TIP`, `TAGS`, `ONTO`, `REPLACED`,
  `PRODUCED`, `RESULT`, `ON`) along with the two things it must not
  over-read — that a missing rebase is missing evidence rather than evidence of
  absence, and that `reachable: false` marks what a rewrite left behind. An
  agent can ask a question without first discovering the schema.

- **The `git` plugin is in the official catalog**, pinned to `git-v1.0.0`
  (`sha256:ce50d72f…`), so a bare `drsg plugin install` offers it beside the
  eight language parsers.

- **A plugin may be dispatched by the shape of the source, not only by a file
  extension.** Routing everywhere else asks what a file is called; a
  repository's history is not a file. A plugin named `git`, when installed, is
  handed a host rooted at the repository's **git directory** and nothing else
  — a *narrower* grant than the working tree every code plugin gets, and one
  the tree's plugins never had: `.git` was always excluded from the ordinary
  walk. Nothing is guessed — with no such plugin installed, a digest simply
  does not read history and says so once.

### Fixed
- **Four log messages had lost their line continuations** and printed runs of
  spaces mid-sentence (`"…from a different directory — file        attribution
  will not line up"`). Three predate this release.

- **A plugin that claims no file extension no longer prints `handles:` with
  nothing after it** on install — it says how it is dispatched instead.

## [2.1.1] - 2026-08-25

### Changed
- **`drsg init` is idempotent — the "make sure drsg is up here" command.**
  The server it spawns is nobody's child: an MCP `http` entry is a connect
  instruction, so no agent client ever relaunches it, and nothing survives a
  reboot. Answering "is drsg up for this repo?" therefore has to be `init`'s
  own job. It now reads the endpoint a previous run recorded in `.mcp.json`
  and probes `GET /health`: a server still answering is left alone and the
  database is never opened (so re-running no longer dies on the single-writer
  lock); one that died is restarted on the *same* address and token, so every
  agent's config stays valid, and without `--force`, so the plane resumes
  from its sync point instead of re-parsing the whole tree. An HTTP probe
  rather than a bare TCP connect, because after a reboot an unrelated process
  may hold that arbitrary port — and if one does, `init` moves to a free port
  and says so. Safe to run from a `SessionStart` hook.
- **`drsg init --addr` falls back to `drsg.toml`'s `[server] addr`**, the way
  `--token` already fell back to `[server] token`. Pinning both keeps a
  repo's MCP endpoint byte-identical across restarts.

### Fixed
- **`drsg init` in a project with no commits.** `serve watch` read HEAD
  before anything else and gave up when there was none, so a directory whose
  first commit was still unborn — the ordinary state of a new project — got
  no plane, no digest, and no watcher, while `init` reported success and
  exited 0 with the real error buried in `logs/`. The tree is now parsed
  straight away, so the plane is queryable the moment `init` returns; the
  watcher then waits for the repository's first commit and rebuilds on it
  (the tree can move between the scan and that commit, and only a real
  commit can be recorded as a sync point) before folding commits as usual. A
  directory that is not a repository at all gets the same initial parse and
  says plainly that nothing will fold into it.

## [2.1.0] - 2026-08-22

### Added
- **`serve --follow` — read-only replicas (arch/01 §9).** A second
  `drsg serve` can mirror a running one for read-scaling across a cluster:
  every write RPC is refused regardless of token, and the replica
  bootstraps from the master's `GET /snapshot` then tails its `GET /ws/wal`
  for new commits, shipped as raw WAL ops so the replica's KV content
  converges byte-for-byte with its source. Every reconnect does a full
  resync from scratch — no partial catch-up. Native-backend only.

## [2.0.2] - 2026-08-22

### Fixed
- **`snippet` reads source from the plane's own root.** It previously
  resolved the node in the named plane but read the file from the
  process-level `source_root`, so a second plane could silently return
  another repository's file at the same relative path. Now uses the
  plane's own `synced_root`, falling back to the process-level tree only
  when neither is available.
- **`serve watch` marks a plane mid-rebuild instead of reporting it
  absent.** A lookup against a plane that `resync` is still refilling used
  to read identically to "not found." The plane now carries a persisted
  `rebuilding_since` marker, and every verb's response notes it — on both
  empty and found answers — until the rebuild completes.

### Performance
- **Wasm plugin compilation runs in parallel.** `Plugins::load` now
  enables wasmtime's `parallel-compilation` feature instead of compiling
  every installed component on one thread; measured 13.8s -> 2.2s to load,
  cutting `serve watch` startup-to-servable from ~15s to ~4s.

## [2.0.1] - 2026-08-19

### Added
- **`drsg init` as a one-command MCP bootstrap.** Repurposed to configure not
  just Claude Code but also Cursor, OpenCode, Gemini CLI, and Codex CLI in
  one pass, probing each tool's config location and writing the MCP server
  entry it expects.
- **`list_planes` surfaces `synced_root`/`synced_commit`.** Callers can now
  match a plane to their working directory and confirm which commit it
  reflects, instead of guessing from the plane name alone.

### Changed
- Repo now gitignores `.mcp.json` and ships its own `drsg.toml`.

## [2.0.0-alpha] - 2026-08-18

The code-intelligence release: a repository becomes a resolved call graph
with no model in the loop, stays synced to every commit, and answers an
agent's structural questions in one round trip. Alpha because the plugin
contract and the agent tools are new surfaces still settling; the graph
engine underneath is the same 1.x core.

### Added
- **Preprocessor plugins (ROADMAP §11).** Sandboxed `wasm32-wasip2`
  components behind a two-phase WIT contract (`parse` chunks in parallel,
  one `assemble` for cross-file resolution), installed by URL or file with
  the SHA-256 pinned and re-checked at every load. The official catalog
  covers eight languages — Rust, Go, TypeScript/JavaScript, Python, Java,
  C, web (HTML/CSS), TOML — each wrapping a canonical parser. The sandbox
  grants three host functions and nothing else: no network, no clock, no
  entropy, fuel and memory budgets. No plugin can call a model — a
  repository that yields only parsed facts is digested without a single
  model call.
- **`drsg serve watch`.** Serve as usual and follow a repository commit by
  commit: changed files re-run through the plugins and fold into the plane
  in place — convergent with a full re-digest, embeddings and foreign edges
  surviving patches — with incremental re-vectorization when the server has
  an embed provider. Every agent answer opens with `synced: commit <sha>`.
- **The agent tools**: `context` · `search` · `describe` · `grep` · `trace`
  · `impact` · `snippet`, identical over MCP and the CLI (`grep`/`snippet`
  live with the server, which knows the source tree). Compact one-fact-per-
  line output under a fixed context budget; ambiguous names return
  candidates; call listings are stated lower bounds backed by the
  `UnresolvedRef` ledger, where every unresolved call carries its reason.
- **Resolution disciplines across all eight parsers**: qualified-name keys,
  file/line on every fact, `_resolved_by`/`_confidence`/`_ref` stamps on
  every resolved edge, receiver typing from declared facts only, closure
  and callback parameters typed by the callee's declared bounds,
  function-as-value and string-literal REFERENCES.
- **RPC + SDKs**: `plane.vectorize` and the `plugin.list` / `plugin.catalog`
  / `plugin.install` / `plugin.remove` methods; all five generated SDK
  clients cover them.
- **Dashboard**: an extensions panel (installed plugins, upgrade/remove,
  install) and a Vectorize button on every plane card.
- **Docs**: two new book chapters (Plugins; Coding Agent) in both
  languages, AGENT-BENCHMARKS.md beside the engine benchmarks, and a
  precision pass across the README, arch/ notes, and the book.

### Changed
- **The MCP `search` tool changed shape** (the major-version reason): it
  was a raw-vector top-k that took an embedding; it now embeds the query
  text server-side (the `[digest]` embed settings) and expands the best
  hit. Callers that sent raw vectors should use `query` plans or `hybrid`.
- `describe` on MCP is now the node-only view of one symbol, matching the
  CLI verb of the same name.

### Fixed
- A configured-but-keyless embedding provider now fails in milliseconds
  with the variable name it wants, instead of hanging a search to the
  300-second request timeout; embedding calls also carry their own
  60-second timeout, and MCP errors render the full cause chain.
- The `semantic_search` definition behind the `search` verb was committed;
  the callers had landed ahead of it.

## [1.7.0] - 2026-08-11

### Added
- **Documents are read everywhere, and become Markdown.** `drsg digest`, the
  `digest` MCP tool and the dashboard upload now share one reader covering
  Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV and PDF, alongside
  Markdown and plain text. Previously only the dashboard read documents, only
  PDF and DOCX, and `drsg digest report.pdf` failed on the first non-UTF-8
  byte. The output is Markdown rather than flat characters, so the model sees
  headings, tables and lists — and the format is detected from the file's
  contents, so a wrong extension still converts.
- **Nodes can be embedded as an agent writes them.** Set `[digest]
  embed_provider` and `/mcp`'s `write_nodes` gives each node a vector built
  from the same recipe and stored in the same property `digest` uses, so an
  agent's writes and a digest's land in one index and one search finds both.
  Off unless configured; a node that already carries a vector is untouched;
  the whole batch costs one provider round-trip.
- **`digest` accepts a `path`** on the stdio MCP server, which runs on the
  agent's own machine. A shared `drsg serve` refuses it — reading any path a
  caller names would let a remote agent pull server files into the graph.

### Changed
- **A query now has a time budget.** `drsg serve` stops one after 60s
  (`[server] query_timeout_secs`, `0` to disable) and answers with the
  retryable `-32002`. Embedded callers are unaffected and still run to
  completion. Cooperative and row-paced: it bounds work that flows, not a
  graph algorithm mid-iteration.
- **Slightly different vectors from `digest`.** Text and embedding now share
  one promotion rule, so numbers, booleans and lists feed the vector where
  only strings did before. Re-digesting a source produces a marginally
  different vector than 1.6.0 — worth knowing because identity matching
  compares against previously written embeddings.

### Fixed
- **`digest` could shadow entities the plane already had.** The bulk path
  writes the external-key index unconditionally, so re-digesting a source, or
  digesting two that name the same entity, overwrote the index entry: the
  original node stayed but became reachable only by id, and every `key(…)`
  read against it silently returned empty. Entities already present are now
  skipped and reported by name. The same fix already landed for `digest.write`
  over `/rpc`; `write_nodes` was never affected.
- **The dashboard's upload filter hid files the server could read** — it
  listed four extensions while the reader accepted twelve.
- **An extensionless upload was refused** as a file type named after the file.

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

[Unreleased]: https://github.com/wangyingsm/dr-strange/compare/v1.7.0...HEAD
[1.7.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.7.0
[1.6.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.6.0
[1.5.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.5.0
[1.2.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.2.0
[1.1.0]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.1.0
[1.0.2]: https://github.com/wangyingsm/dr-strange/releases/tag/v1.0.2
