# Changelog

All notable changes to Dr Strange are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.24.0] - 2026-10-28

Three things reach past the server this release. The engine opens a graph
inside a Python process with no socket in between; the ops plane 2.23.0
introduced now arrives from an ordinary digest rather than being asked for; and
the accuracy benchmark adds two more tools to score, one of which is no tool at
all — a plain text search, which turns out to match this project's recall
exactly and assert 133 calls that do not exist.

### Added

- **`drsg-embedded` — the engine in your own process, as async Python.** One
  file on disk, opened directly: `db = await drsg.open("graph.drsg")`, then the
  surface the MCP server and the CLI already use — `db.context(plane, name)`,
  `db.impact(plane, name, depth=3)`, `db.deployed(plane)`. Every method is a
  coroutine and every one does its work on a blocking thread, because a point
  lookup here is microseconds of CPU rather than I/O to wait on, and running it
  inline would stall every other task for exactly that long. Calls name their
  plane rather than returning a handle, since a plane borrows the database it
  came from and a Python object cannot hold a borrow. This wraps the
  **engine**, not the digest: building a plane needs the sandboxed wasm parser
  plugins, which would put tens of megabytes into every wheel, so build with
  the CLI and read from here. Alpha — the surface above is settled, and
  `cypher`, the change feed and bulk writes are next. Wheels build for five
  targets (Linux `x86_64`/`aarch64`, macOS Apple Silicon/Intel, Windows
  `x86_64`) and are tested in CI against the wheel rather than the source tree;
  they publish once a `PYPI_TOKEN` secret exists. Nothing is on PyPI yet.
- **An ordinary digest writes the ops plane.** The Compose, Kubernetes and
  Terraform readers are routed by the normal rules — a compose file by its
  name, a `.tf` by its extension — so their facts used to arrive mixed in with
  the code's, where they do not belong: a workload and a function answer
  different questions, and a chart changes on a different cadence from the code
  it deploys, so re-reading one must not invalidate the other. They now land in
  `<plane>_ops`, the plane `deployed` already reads. Nothing has to ask for it,
  and the split is made on the one thing every fact carries — `_generated_by`,
  the plugin that produced it — never on a label, because `Namespace` means one
  thing from the Kubernetes reader and another from a language parser that
  borrows the word. An incremental fold and a `--force` rebuild route facts the
  same way, so the plane cannot drift from the tree depending on how it was
  built.
- **`drsg token` reads the bearer back, masked.** `init` stopped printing the
  token (below) and printed a `jq` pipeline over `.mcp.json` in its place,
  which needed `jq` installed and said nothing useful when no `init` had run
  here. This is that read, as a command: bare, it masks —
  `AAAA…LmXw <40 characters>`: enough to tell one token from another,
  never enough to use, and a token too short for its ends to be given away
  cheaply is masked whole. `--unhide` prints it for command substitution, which
  is the form `init` now prints: `export DRSG_TOKEN=$(drsg token --unhide)`.
- **`[plugins] hostcall_mb` — what one call may copy out of the sandbox.**
  `parse` returns one file's partial and stays far below wasmtime's 128 MiB,
  but `assemble` returns the whole tree in one value, so a large repository is
  the first to pass it. The error now names this setting. Leave it out, or set
  `0`, and wasmtime's own limit applies; there is no environment variable for
  it.

### Changed

- **A bulk load is 24% faster again** — 343 → 426 K rows/s, on top of the
  roughly doubled throughput the `bulk_load` fast path already recorded. Two
  changes: the write batch is handed buffers it already owns instead of copies,
  and the CRC reads eight bytes a round off a table rather than one byte, which
  had been a quarter of a load. Measured as an interleaved old/new A/B in one
  window, so the delta is the code and not the hour of day.
- **The accuracy benchmark scores five tools, and a text search is one of
  them.** GitNexus 1.6.12 joins the recall table, and beneath it a baseline
  that needs nothing installed, so the comparison's floor is always
  reproducible. Read the last two columns together, because they are the
  finding: a plain text search matches this project's recall exactly — 65 of 67
  — and asserts 133 calls that do not exist. Recall alone therefore does not
  separate a resolved call graph from `rg`; everything that does is in the row
  beneath it and in the honesty arm. That arm now reports two rates per tool
  rather than prose — an **unresolved-reference disclosure rate** (of the
  ambiguous sites where no edge was drawn, how many were *named*) and an
  **ambiguity misjudgement rate** (of the sites where a guess was available,
  how many were guessed at with certainty) — plus a third, traversal-boundary
  disclosure, recorded as a property of each tool's own reach answer. drsg
  discloses 24/47 and misjudges 0/47; GitNexus 2/23 and 16/39; text search
  0/11 and 28/39. The two rates pull in opposite directions by design, which is
  why neither is reported alone.
- **The engine benchmark report dates each column from the data it was
  measured from**, not from the file's mtime, and keeps the sections it used to
  delete on regeneration. Both harnesses stamp their results with the
  measurement time to make that possible.

### Fixed

- **`drsg init` wrote a Codex config it could not read back.** The pre-existing
  `.codex/config.toml` was parsed with `str::parse::<toml::Value>()`, and under
  toml 1.x `FromStr` reads one *bare value* rather than a document — so a real
  config ends at its first table header and the rest is rejected as unexpected
  content. Every Codex user with an existing config therefore had `init` refuse
  it as invalid TOML. Parsed as a document now.
- **A scanned PDF is named in the digest report instead of being dropped.** A
  page with no text layer is not a file we failed to recognise; it is one we
  read and found nothing in. The conversion error was flattened into a string,
  so the bytes then failed the UTF-8 fallback and the file left no line at all
  — a digest quietly lost it. The typed error is kept in the chain now, and the
  report says which pages need OCR.
- **`drsg init` no longer prints the bearer token.** The export hints read it
  back from `.mcp.json` rather than putting the secret in the command's own
  output.
- **The Go SDK reports a socket deadline as the context's.** The only deadline
  on that socket is the one taken from the context, and the two are separate
  clocks for one instant, so a handshake read could fail on the socket's before
  the context's timer had set `ctx.Err()`. Leaking `i/o timeout` there left a
  caller unable to tell its own deadline from a server that had gone quiet, and
  which of the two it saw depended on a race.

### Security

- **tokio 1.54.1**, for RUSTSEC-2026-0384.
- **Four zstd advisories cleared** — RUSTSEC-2026-0391, -0398, -0403 and -0410,
  reached through `jieba-rs → include-flate → zstd`. `cargo update` could not
  cross the majors involved; `include-flate` 0.3.5 moved to `zstd ^0.14`, which
  lands all three minimums (zstd 0.14.3, zstd-safe 8.2.0, zstd-sys 2.1.2).
- Dependencies: reqwest 0.13.4 (which also drops a duplicate copy), anydoc
  0.2.4, toml 1.1.8, and the cargo group. The three `zip` advisories recorded
  in `deny.toml` are **not** cleared: anydoc 0.2.4 still requires `zip ^8.6`.

## [2.23.0] - 2026-10-21

The thesis of this project is that a commit and a production incident are two
ends of one graph. This release makes the first half real: a repository's
declared topology — the workloads it runs, the images they carry, the
infrastructure beneath them — becomes a plane of its own, and one verb joins it
back to the history plane beside it. The other half is measurement: an accuracy
benchmark that scores this project against two others over eight languages,
with every answer key checked by a compiler rather than by us.

### Added

- **`deployed` — what commit a running thing was built from, and what runs a
  commit.** It reads the ops plane (`<plane>_ops`: namespaces, workloads,
  containers, images, infrastructure) against the history plane
  (`<plane>_git`), joined by the revision an image's tag carries. Name a
  workload, container, resource or image key for the commit behind it; name a
  revision for everything declared to run it; name nothing for every image at
  once, each line marked `→` where the join held and `—` where it did not, so
  one glance says how much of a plane is traceable. Planes are hard namespaces
  and a cross-plane edge is rejected at storage, so this is a read of two
  planes rather than an edge between them; `arch/10-ops.md` §9 records why, and
  why the `portals` concept stays unshipped. MCP and the CLI, never
  `openrpc.json`.
- **Every way that join fails is named rather than answered with nothing.** A
  floating tag (`latest`, a version number), a digest that pins the bytes and
  says nothing about provenance, a revision no commit of this history holds, an
  abbreviated revision that fits two commits, and a missing history plane are
  five different facts; a verb that returned nothing for all of them would
  leave a reader unable to tell a gap from a fact. A third-party image is
  deliberately not guessed at — nothing in a manifest distinguishes
  `postgres:17` from a locally built `myapp:17` — so the verb claims an image
  came from elsewhere only when a revision said so.
- **The ops graph's model** (`arch/10-ops.md`): the nodes and edges a manifest
  becomes, what the container image joins and where it cannot, and the rule
  that ConfigMap values are recorded while Secret values never are. The three
  readers that write it — Compose, Kubernetes and Terraform — ship from the
  extensions repository and install from the plugin catalog.
- **A compose file is routed by its name.** `compose.yaml`,
  `docker-compose.yml` and their variants reach the Compose reader without
  `--handler`, the way the manifests the host reserves a name for already did.
- **An accuracy benchmark with a cross-checked ground truth.** Eight language
  fixtures whose answer keys are each verified by an oracle that is not this
  project — Roslyn's semantic model, `callgraph` over Java bytecode,
  `sys.setprofile`, the TypeScript checker, `javap -c`, `objdump -d -r`,
  `cargo expand`, the Kotlin compiler — and four task sets: who calls a symbol,
  what a change would reach, which types are referenced across files, and how
  same-name symbols are told apart. Three tools are scored over the same
  fixtures, identity is by `file:line` and never by name, and the harness bugs
  that flattered this project are written down beside the numbers.
- **`drsg init` configures four more agents.** Trae and Kimi Code each get
  their own MCP config; Qoder and CodeBuddy read the `.mcp.json` that is
  already written, and `init` now says so rather than writing a second copy.
- **A contributor guide** (`CONTRIBUTING.md`) stating what CI already enforces,
  with issue and pull-request templates.
- **A landing page** for the code-intelligence case, served as the site root.
- **The provenance bundle ships beside the archives.** It can then be verified
  offline against the archive alone, and an audit that reads releases — OpenSSF
  Scorecard among them — can see provenance that was previously recorded only
  against the repository.

### Changed

- **rmcp 2.2 → 3.** `ServerInfo` is now `ServerConfig`: the same
  `InitializeResult` under a name that does not collide with the protocol's own
  `serverInfo` field. The advertised protocol version moves to `2026-07-28`;
  existing clients are unaffected, since negotiation answers with the client's
  own version whenever the server supports it, and it supports every known one.
- **nom 7 → 8.** Combinators are parsers rather than callables, so the Cypher
  grammar drives them with `.parse(input)` throughout and `tuple((…))` is gone
  — a tuple is itself a parser now. The grammar is unchanged, and its 195 tests
  pass as they stood.
- **sha2 0.11, jieba-rs 0.11** and fourteen patch-level crates.
- **The GitHub Actions set** moves up a major in several places: checkout v7,
  upload-artifact v7, download-artifact v8, setup-java v6, setup-node v7,
  setup-uv v10, and the Docker actions to v4/v6/v7. That also clears the
  Node 20 deprecation warning every SDK job was carrying. The Java SDK builds
  against Maven 3.10, JUnit 6 and Jackson 2.22.

### Fixed

- **Five RUSTSEC advisories.** `rustls-webpki` 0.103.16 and `webpki-roots` /
  `webpki-root-certs` 1.0.10 reject certificate paths through the distrusted
  emSign intermediates whose private keys are now well known
  (RUSTSEC-2026-0381, -0382, -0383). `wasmtime` 48.0.7 fixes a panic lowering a
  host-created component-model stream into a guest expecting another payload
  type, and an aarch64 `br_table` that could jump to the wrong target
  (RUSTSEC-2026-0378, -0379).
- **`drsg init` ignores the temp sidecars a running server leaves.** A `.hnsw`
  or `.bm25` file caught mid-write looked like an untracked file worth
  committing.
- **The toolchain pin says which master commit it is.** Dependabot rewrites the
  sha and leaves the comment beside it, so `# master 2026-09-11` had come to
  describe a commit of 2026-10-16.

## [2.22.0] - 2026-10-17

`drsg init` now configures the agent you run, not only the one this repository
has seen before. The rest is supply chain: release archives carry signed
provenance, and the advisories an audit found are gone.

### Added

- **`drsg init` configures an agent this machine has.** It wrote a config for
  Cursor, OpenCode, Gemini CLI or Codex CLI only when the repository already
  carried that tool's marker — which a fresh checkout never does. It now
  accepts either signal, the repository's or the machine's, so a first run
  sets up the agent you actually use. A tool neither names is still left
  alone.
- **The installers fall back to a mirror.** `--mirror <base-url>`
  (`DRSG_MIRROR`, `-Mirror` on Windows) names a mirror holding the same
  layout. GitHub is tried first and the mirror takes over only if that attempt
  is failing or crawling; the SHA-256 is verified whichever source answers, so
  a mirror is trusted for availability and never for content. `--no-mirror`
  forbids it. Without a mirror configured nothing changes — a slow download is
  left to finish rather than abandoned.
- **Release archives carry SLSA build provenance**, signed through Sigstore
  with no key to keep. Verify a download against the workflow and commit that
  produced it:
  `gh attestation verify <archive> --repo wangyingsm/dr-strange`.
- **`npx drsg`.** The packaging for six npm packages — a wrapper and one
  prebuilt binary per platform — and a release job that publishes them once an
  `NPM_TOKEN` secret exists. Nothing is on npm yet.
- **A security policy** (`SECURITY.md`), **Dependabot** across seven
  ecosystems, an **OpenSSF Scorecard** run, and **CodeQL** over the Rust, web,
  Python and workflow sources.

### Fixed

- **The Python client refuses TLS below 1.2.** `ssl.create_default_context`
  verifies certificates and hostnames but leaves the floor wherever the
  platform's OpenSSL puts it, which on older builds still admits TLS 1.0 and
  1.1 — both withdrawn by RFC 8996.
- **The TypeScript client strips trailing slashes without backtracking.**
  `/\/+$/` over a caller-supplied base URL is quadratic on a string of many
  slashes; a scan from the end says the same thing in linear time.
- **The Java client builds on a patched Jackson** (2.17.2 → 2.18.11), clearing
  fifteen advisories against `jackson-core` and `jackson-databind`.
- **The dashboard build pulls no advisory-bearing package.** `devalue`,
  `nanoid` and `source-map-js` carried nine advisories between them; `dist/`
  is embedded in this binary, so a build dependency reaches users.
- **A write token goes to the job that writes.** `release.yml` and `docs.yml`
  granted `contents: write` and `pages: write` at the top level, handing a
  write-scoped token to every job including the ones that only compile.

## [2.21.0] - 2026-10-16

The web UI shows what the database already knew: where each plane came from,
and which of the routes a plane states actually meet. Both read existing
JSON-RPC methods, so `openrpc.json` is unchanged and no SDK goes stale.

### Added

- **The dashboard states each plane's provenance.** A plane served from a
  mounted database carries the mount's name; a plane that attaches others
  carries one tag per attachment, those on another instance marked *remote*.
  `plane.list` already returned the mount and `plane.attachments` the rest —
  the dashboard simply never showed them.
- **A "Mounted databases" section.** When this process serves other stores
  alongside its own (`db.mount`), each is listed with its path, the planes it
  contributes, and whether those planes carry the mount's name as a prefix,
  together with the directory mounts are confined to. Absent when nothing is
  mounted.
- **A Wires tab in Explore.** It reads the routes a plane states — `serves` on
  a handler, `requests` on a caller — and pairs them, over the same selection
  the `wire` verb uses. A request nothing answers is marked rather than hidden.
  Routes are shown both normalized and as written, filtered by verb or path
  substring, and every symbol centers that node on the canvas. The matching
  rule is core's own (equal paths; verbs equal or either one `*`) and is pinned
  by its own tests so the browser's copy cannot drift from `Stated::answers` in
  silence.

## [2.20.6] - 2026-10-16

Documentation and the SDK manifests only; no shipped crate changed, so the
binary behaves exactly as 2.20.5's.

### Changed

- **The six SDKs state the workspace version.** They carried `2.0.0a0` and
  `2.0.0-alpha` while the workspace was at 2.20.x; each manifest — Python's
  `pyproject.toml` and `__version__`, TypeScript's `package.json`, Java's
  `pom.xml` — now says the version this repository is at, and `sdk/README.md`
  says so too. None of them is published to a registry, so a checkout is still
  how you get one; what changes is that the number no longer contradicts the
  tree it came from.
- **The SDK chapter says how to depend on each client.** It said only "vendor
  the directory or depend on it in place". It now gives the step per language —
  `pip install …/sdk/python`, a `replace` directive for Go, `./mvnw install`
  then `io.github.wangyingsm:drsg:<version>`, `bun run build` then a `file:`
  dependency — and states plainly that no registry release exists yet.

### Fixed

- **The contract documents describe what ships.** `arch/04`, `arch/05`,
  `arch/06`, `arch/08` and the book's MCP, web-UI, getting-started, appendix
  and query-language chapters record the behaviour the hardening releases
  actually landed: what an MCP tool confirms before it destroys, that `grep`
  and `snippet` read only inside the tree they belong to and how that differs
  between the stdio and shared servers, that every tool call carries one
  deadline covering its wait and its run, which bind addresses require a token
  and where that token goes, that a wrong token more than five times earns a
  `429` with a doubling `Retry-After`, which provider a request may name, and
  why `digest.run` over JSON-RPC is deliberately *not* confirm-gated the way
  the MCP tool is.
- **Both editions of the book say the same thing.** Four passages had reached
  the English edition only; the Chinese ones are written, each against the code
  it describes rather than translated on faith.

## [2.20.5] - 2026-10-16

Like 2.20.4, nothing under `crates/` that ships changed: `release.yml` builds
only `dr-strange-cli` and `dr-strange-mcp`, and the benchmark harness is
neither. The binary behaves exactly as 2.20.4's.

### Fixed

- **The vector benchmark now checks that it was right, not only that it was
  fast.** It timed ANN queries and never scored them, so an index returning
  the wrong neighbours quickly read as a win. `drsg-bench gen` writes an exact
  brute-force top-100 for a sample of the vector queries, computed from the
  values it actually wrote to the dataset; every engine scores its ANN answer
  against that one shared oracle, and a `vector_recall` row carries the mean
  recall and the `k` it was scored at. One oracle serves any `k` up to 100
  without regenerating the data.
- **The comparators measure what drsg measures.** Each loads through its own
  bulk path — Kùzu's `COPY`, SQLite's `executemany`, Neo4j's `UNWIND` — and the
  report says where the engines genuinely differ instead of presenting unlike
  work as like. The load row now also says where each engine parses its CSV:
  SQLite's timed `executemany` parses lazily, which is not the same work as a
  loader that parses up front.
- **The evidence behind the published table is kept.**
  `benchmarks/results/*.json` — per-op figures, pass count, recorded spread —
  is no longer ignored, so the table in `BENCHMARKS.md` can be checked and
  reproduced rather than taken on trust. `AGENT-BENCHMARKS.md` also says what
  that benchmark cannot show.

## [2.20.4] - 2026-10-16

Nothing under `crates/` changed in this release: the drsg binary behaves
exactly as 2.20.3's. What changed is the six SDKs, which ship from this
repository rather than from a language registry, so they reach you through a
tag.

### Fixed

- **A watch client's token no longer rides the URL.** Every SDK put the bearer
  token in the change feed's query string — `/ws?token=…` — which is a token in
  proxy logs, server logs and browser history. The Go, Python, TypeScript, Java
  and C clients now send it as an `Authorization: Bearer` header on the
  WebSocket upgrade, and refuse a token holding a control character before a
  socket is opened at all — a CR or LF would end the header line and forge
  another. The server has accepted both forms all along, so a newer client
  works against an older server. The TypeScript client chooses by
  **capability**, not by sniffing the document: Bun and Node send the header, a
  browser window, a worker and Deno cannot set one on a `WebSocket` and still
  fall back to `?token=`.
- **The change feeds let go.** The Go subscription honours its context and
  releases a closed peer; the C watch can be stopped from outside and withdraws
  its socket when a cancel beats the connect; the Java feed drops a
  `plane.change` carrying no payload instead of handing a subscriber a null
  record, and tells a caller when its feed has died.
- **Transports fail in their own vocabulary.** The Python client raises
  `DrsgProtocolError` rather than leaking a `websockets` exception, and bounds
  what a fragmented message may buffer. The TypeScript client names its timeout
  and ships declarations that match it. A Java watch timeout names the
  configured duration whichever of its two timers fired — its own handshake
  deadline or the JDK's `connectTimeout` — where before one of them left a bare
  "request timed out" to reach a caller who had just set 300ms.
- **Clients can be closed and configured.** The Java client gains `close()` and
  an options constructor that reaches `timeout` and `httpClient`, which
  `Client.call` being protected had made unreachable from the typed API. The C
  client states its one-client-per-thread contract.
- **The C code generator bounds its fixed parameter arrays.** A future method
  with more parameters than they hold becomes a build failure naming the method
  rather than a silent stack overwrite. The generated output is unchanged.

## [2.20.3] - 2026-10-15

### Fixed

- **A durable commit can no longer report `Err`.** `WriteTxn::commit`
  committed the KV and then applied the buffered vector index events with
  `?`: the first failure returned `Err` for a write that was already durable
  and visible, skipped every remaining vector and keyword event, and left the
  in-memory registries quietly behind the KV. The divergence could outlive the
  process — a clean close saved the diverged registry as a sidecar stamped with
  the current sequence, and the next open loaded it back as fresh instead of
  rebuilding. Nothing after the KV commit may become an `Err` now: every
  pending event is applied, one failure never skipping the rest, and a failed
  vector event marks the registry *diverged* — it keeps serving, which beats
  no answer, but is never persisted at that sequence, so the next open rebuilds
  from the KV, which is always the truth.
- **A restore leaves the live registries matching what it restored.** Restore
  only loaded a snapshot's HNSW and BM25 frames when the database had a sidecar
  *path* to write them to. An in-memory database has none, so it kept its
  pre-restore, empty registries: keyword search returned nothing and vector
  search silently fell back to brute force. The registries are now set
  whether or not a sidecar path exists — from the shipped bytes when they parse
  and carry the restored sequence, otherwise rebuilt from the just-restored KV.
  The sidecar writes come last and are best-effort: a sidecar is a cache of the
  registry, so a failed write is a warning rather than an `Err` from a restore
  that has already landed, which would have the caller retry a restore the
  non-empty check then refuses.
- **A replica's restore lands at its master's sequence.** A fresh follower's
  own bootstrap takes sequence 1, and a restore that allocated its own sequence
  left the replica *behind* a young master — so the master's very next live
  batch was at or below the follower's `committed_seq`, `apply_replicated`
  refused it as a replay, and the follow loop answered with a full resync,
  forever. The write path now carries the sequence a restore must land at (or
  above) and the native engine commits at `max(next, floor)`; sequences still
  only ever advance.
- **A follower's searches follow.** `apply_replicated` landed each batch in the
  KV and dropped the graph cache, but never touched the in-memory HNSW and BM25
  registries, so a `serve --follow` replica answered vector and keyword
  searches from whatever it rebuilt on open: nodes created on the master never
  became searchable, deleted ones were hidden only by the snapshot filter, and
  an index declared after the follower opened did not exist for it at all. Each
  replicated batch is now mirrored into both registries.
- **A restored database no longer reports itself empty.** The per-plane summary
  counters are maintained by `WriteTxn::commit`, which the restore path does not
  go through — it writes frames with raw graph inserts — so the target kept the
  zero row its bootstrap wrote and `db.stats` and `drsg stats` reported an empty
  database over a full one. Each plane is recounted inside the restore
  transaction.
- **Opening a database commits nothing unless something is missing.**
  `Database::init` ran two bootstrap write transactions on every open even when
  both had nothing to do, advancing the commit sequence by two. On a replica
  that is a correctness bug rather than a blemish: its sequence is the master's,
  landed verbatim, so each restart of `serve --follow` pushed it two past the
  master. It also cost every embedded open two fsyncs.
- **Dropping a plane drops its index declarations.** `drop_plane` swept the
  plane's rows by key prefix and deleted its counters row, but the `vidx:` and
  `kidx:` declarations live in meta under their own prefixes, as do the live
  registry entries. A stale handle still listed the dropped plane's indexes, the
  sidecars kept carrying them, and every later open rebuilt them over nothing —
  for a plane id that is never reused.
- **The redb-only build compiles again.** A test-only fault seam and the
  follower integration test are native-only, and are now gated like their one
  caller.

## [2.20.2] - 2026-10-15

### Security

- **The plugin sandbox is patched against
  [RUSTSEC-2026-0352](https://rustsec.org/advisories/RUSTSEC-2026-0352).**
  `wasmtime-wasi` 48.0.5 allowed a filesystem sandbox escape when renaming a
  symlink with a trailing slash
  ([GHSA-m9f7-9w4p-gv38](https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-m9f7-9w4p-gv38)).
  A preprocessor plugin is a wasm component drsg downloads and runs over a
  source tree, and its preopened directory is the whole of what stands between
  it and the rest of the filesystem — so of the sandboxes drsg relies on, this
  is the one that matters. 48.0.6 closes it. No drsg code changed, and no
  installed plugin needs reinstalling.

## [2.20.1] - 2026-10-15

### Fixed

- **`unmount` now frees the store it stopped serving.** A mounted repository's
  watcher held a handle to its database, so unmounting removed the mount, said
  it had released the lock, and released nothing: the orphaned thread kept the
  exclusive lock — and kept folding commits into a database this process no
  longer served — until the process exited. Nothing else could open that
  directory, which made unmounting a repository in order to work on its store
  elsewhere, or re-mounting the same path, fail on a lock this server itself
  held. A mount's watcher now **borrows** its database: it takes a handle per
  fold, never across its sleep, and stops once the mount is gone. `unmount`
  drops the last handle itself rather than handing it to the caller that asked
  for the lock to be released, waits briefly for a fold in flight, and answers
  `freed` — so an operator is told whether the store came free. The host's own
  watcher still holds its database, which outlives every watcher anyway.

## [2.20.0] - 2026-10-15

### Added

- **`mount`: one process serving many databases.** A repository digested into
  its own store is the ordinary case, and a machine ends up with dozens —
  thirty-four, in the report this comes from, each needing its own `drsg serve`,
  its own port and its own cache, while a client configured for one port reached
  one of them. `drsg mount <path>` serves another database's planes on a
  **running** drsg, keeping the planes' own names, so `--plane svc-orders` works
  exactly as before. Nothing is copied and nothing is re-digested. From issue
  #44. `db.mount` / `db.unmount` / `db.mounts` on the JSON-RPC surface (42
  methods now, with the six SDKs regenerated), the same three as MCP tools, and
  `drsg mount` / `unmount` / `mounts` on the CLI — those three alone among the
  CLI's verbs open no database, because the server holds the store's lock, so
  they speak its RPC instead.
- **A mount is not a merge, and that is the design.** Node and plane ids are
  allocated per database and collide freely across them, so nothing joins two
  stores: a request reaches exactly one, and the only thing shared is the name
  it was asked for. Each mount therefore keeps **its own writer slot** — which
  is the point for a machine folding commits in thirty-four repositories at
  once, where a single store would queue every fold behind one gate — its own
  lifecycle (dropping a repository is an unmount, not a range-delete inside a
  shared file) and its own blast radius.
- **Every verb follows, because a plane name is resolved once per request.**
  `plane` is the one key every method and tool uses for "the plane to act on",
  so the RPC resolves it at dispatch and the MCP layer in one place; all 39
  pre-existing RPC methods and 26 tools answer across mounts untouched.
  `context`, `grep` and `snippet` each answer in their own repository's tree,
  and `impact --across all` crosses every mounted database in one process —
  issue #43's cross-repository question with no attachment setup at all.
- **`serve watch` keeps every mounted repository current**, on the tree its own
  plane records: the part that actually retires the other thirty-three
  processes. One hook covers a mount replayed at startup, named on the command
  line, or arriving over the wire hours later. A mount is written down on the
  host's own plane, like an attachment, and served again after a restart; a
  recorded directory that has moved is reported and skipped rather than taken
  as a reason not to start.
- **Confined, because mounting opens a file a caller names.** `db.mount` is
  admin-tier *and* restricted to one granted directory (`[server] mount_root`,
  the operator's home by default) — a plane inside a mounted store records a
  `synced_root` that `grep` and `snippet` would then read, which is the
  escalation `TreeAccess` exists to prevent. That same directory becomes a
  second readable root for those verbs, so a mounted repository's files are
  reachable without weakening the rule. A local process (stdio, the CLI) is
  unconfined, already running as the user whose files these are. Two refusals
  come from the engine itself: a store another process holds, by its exclusive
  lock, and a path that does not exist — mounting opens a database, it does not
  create one.

### Security

- **`rustls` moves to 0.23.46**, closing four advisories published since the
  last release: `Debug` impls on client session values disclosing the TLS 1.2
  master secret or the TLS 1.3 resumption PSK ([RUSTSEC-2026-0346]), buffered
  application data sent to the outer server after an ECH rejection
  ([RUSTSEC-2026-0347]), brotli certificate decompression allocating up to a
  gibibyte before authentication ([RUSTSEC-2026-0348]), and an unauthenticated
  `ServerHello` downgrading later connections to classical key exchange
  ([RUSTSEC-2026-0349]).

### Fixed

- **A mount is asked its own database, not a list cached when it was mounted.**
  A plane renamed or dropped through this instance would otherwise leave the
  recorded list answering for a name that is no longer there.
- **Mounting needs a storage backend**, and so does replaying a recorded mount:
  a build with no backend feature had `Instance::replay` calling a gated
  `mount`, which the feature gate caught before release.

[RUSTSEC-2026-0346]: https://rustsec.org/advisories/RUSTSEC-2026-0346
[RUSTSEC-2026-0347]: https://rustsec.org/advisories/RUSTSEC-2026-0347
[RUSTSEC-2026-0348]: https://rustsec.org/advisories/RUSTSEC-2026-0348
[RUSTSEC-2026-0349]: https://rustsec.org/advisories/RUSTSEC-2026-0349

## [2.19.0] - 2026-10-14

### Added

- **`impact --across`, which says which *other repositories* a change reaches.**
  Dozens of business repositories depend on a shared library, and "which of them
  would this change break" had no answer: a radius walked in one plane stops at
  the repository boundary. It is answered by two keys both sides already state —
  the **coordinate** a build declares (`Manifest.declares` in the library, a
  `DEPENDS_ON` edge carrying `version` and `scope` in the dependent) and the
  **symbol** a dependent's source stands in for (the same key, labelled
  `External`). Two plugins asserting the same foreign key agree rather than
  collide, so a stand-in in one repository and the declaration in another are
  the same key by construction — which is why this needed no plugin change and
  no new RPC method. The keys offered are the changed symbol's *and* every key
  its local radius holds, breadth-first, so a repository referencing a subclass
  of the changed class is found too. Nothing is drawn and nothing is stored, as
  with `wire`: matched per call, so the answer can state what it could not read.
  MCP tool parameter and `drsg impact … --across` alike. From issue #43.
- **Two scopes, because two environments ask this differently.** `--across`
  reads the planes this one attaches — explicit, and the only scope that reaches
  another drsg. `--across all` reads every plane this instance holds, so a
  machine handed thirty repositories needs no setup to be asked across them:
  each one already stated its relationships in its own build and imports, and
  requiring an attachment per pair would be setup to record what is written
  down. Either way the answer names the graphs it read and the ones it could
  not, and three outcomes stay apart: a repository that declares the dependency
  and references the radius, one that declares it and reaches none of it (at
  whatever version it pinned), and one holding a key *without* `External` —
  this code again, vendored or digested twice, reported as `holds its own`
  rather than counted as a user.
- **The Maven coordinate needs no bridge to the package it cannot be mapped
  to.** `com.google.guava:guava` and `com.google.common.…` cannot be joined
  without a guess, which is why the `deps` plugin records a build dependency and
  declines to connect it to any import. Asked standing in the library there is
  nothing to guess: its own `Manifest.declares` already *is* the coordinate it
  publishes, and a dependent's manifest keys its declaration the same way. The
  coordinate is read from the plane being asked about rather than mapped onto
  it, so the plugin's documented gap stands untouched.

### Changed

- **`impact` takes a request rather than a plane handle.** `compact::impact`
  now reads a `&Database` and a `compact::Impact`, the same shape `wire` takes,
  because a verb that may cross graphs must be able to open more than the one
  it was handed. Asked for no scope, its output is byte-identical to 2.18.0.

### Fixed

- **An empty plane is no longer a graph an answer names.** Every database
  carries a `startup` plane whether or not anything was digested into it, so
  `--across all` named it on every call and counted it among the graphs read.
  An empty plane cannot hold a key of any radius; it is skipped by emptiness
  rather than by name, since a plane called `startup` may well hold a real
  digest.

## [2.18.0] - 2026-10-13

### Added

- **`wire`, an agent verb that matches an HTTP route to the code on both sides
  of it.** A page calls `/api/v1/orders/42`; a controller declares
  `@GetMapping("/api/v1/orders/{id}")`; the graph held both files and no
  connection between them, because a call edge needs a callee the parser can see
  and an HTTP request has nothing between its two sides but a string. The
  language plugins now record what the source *states* — `serves` for a
  declaration, `requests` for a call, each a `VERB /path/{}` with every variable
  segment reduced to `{}` — and `wire` computes the correspondence per call.
  Nothing is drawn into the graph: no `Route` node, no `SERVES` edge, because a
  synthesised edge would freeze a guess while a verb asked on demand can report
  what it could not match — a request reaching no declaration, a declaration
  nothing asks for (a dead endpoint, useful in itself), and a side that states no
  verb, which matches every verb on the path and says so. Give it a path for both
  sides of it, a handler to learn who asks, or the function making a call to learn
  what serves it. MCP tool and `drsg wire` alike. From issue #42.
- **`attach`, which lets a verb cross from one plane to another.** A frontend and
  the service it calls are separate repositories, so they are separate planes,
  and an attachment records the one fact that joins them: *this plane may ask that
  one*. It is a single plane property — a name, the plane it names on the far
  side, and a URL when another drsg serves it — and carries **no token**, because
  attached graphs belong to one environment and a caller presents the token it
  already holds, so nothing secret is written into a graph. It carries no
  vocabulary of its own either: what the question is, and what key the two sides
  match on, belongs to the verb asking it. `attach` / `detach` / `attachments`
  are CLI subcommands and MCP tools, so an agent can wire two repositories
  together inside one session; `--both` records the way back for a local pair,
  and `attachments --probe` asks each remote one whether it answers now.
- **`wire` crosses drsg instances over the RPC that already existed.** A plane
  another drsg serves is read with `plane.query`, the question travelling as a
  serialized `LogicalPlan` — so federation added no protocol method and no SDK
  regeneration. One plan serves both sides of the boundary, so a remote graph is
  asked exactly what a local scan reads and the two cannot drift, and a plane of
  any size answers with its routes rather than with itself. An attachment that
  cannot be reached is named with the reason rather than skipped, and every row
  names the graph it came from. The address guard stays on, re-permitted for
  exactly the host the attachment names: a hostname is resolved once, so neither
  a later answer to that name nor a redirect can move the request to an address
  the operator did not name.
- **`plane.attach`, `plane.detach` and `plane.attachments` on the JSON-RPC
  surface**, with the six SDKs regenerated. Plane lifecycle, beside
  `plane.create` and `plane.set_props`: the record is the plane's own property,
  and going through these rather than `plane.set_props` is what validates it and
  keeps two concurrent attaches from losing each other. They return the record
  itself, not the line a terminal prints.

### Fixed

- **A node id means nothing across two drsg instances.** `wire`'s symbol match
  keyed rows by bare `NodeId`, which is unique within one database and arbitrary
  across two — so a row read over the wire could match a local symbol by
  coincidence. The identity is now the pair of graph and id throughout.
- **The Python SDK's generator emitted a keyword as a parameter name.** The
  OpenRPC param `as` became `def plane_attach(self, …, as=None)`, a syntax error
  that stopped the module importing. The generator now gives any Python keyword
  the trailing underscore PEP 8 prescribes, leaving the wire name untouched.
- **The Zig SDK builds on Zig 0.17**, which removed `@cImport`. `drsg.h` is
  translated by a `b.addTranslateC` build step instead; 0.16 and 0.17 both build
  and pass the e2e suite.

## [2.17.1] - 2026-10-11

### Security

- **The plugin sandbox moves to wasmtime 48.0.5**, which closes seven
  advisories published after 48.0.3 shipped in 2.17.0: wrongly typed
  WebAssembly tag imports and missing rooting across `try_call`, each
  corrupting the GC heap
  ([RUSTSEC-2026-0325], [RUSTSEC-2026-0326]); an unvalidated async-lifted
  callback result count overflowing the native stack ([RUSTSEC-2026-0327]);
  WASI preview 0's `poll_oneoff` circumventing fuel accounting
  ([RUSTSEC-2026-0321]); excessive host allocation for a guest without stdio
  ([RUSTSEC-2026-0322]); and two more ([RUSTSEC-2026-0323],
  [RUSTSEC-2026-0324]). drsg runs third-party parser plugins inside that
  sandbox, so these are the class of bug it exists to contain. A patch release
  of the same minor: no API and no minimum-Rust change.

[RUSTSEC-2026-0321]: https://rustsec.org/advisories/RUSTSEC-2026-0321
[RUSTSEC-2026-0322]: https://rustsec.org/advisories/RUSTSEC-2026-0322
[RUSTSEC-2026-0323]: https://rustsec.org/advisories/RUSTSEC-2026-0323
[RUSTSEC-2026-0324]: https://rustsec.org/advisories/RUSTSEC-2026-0324
[RUSTSEC-2026-0325]: https://rustsec.org/advisories/RUSTSEC-2026-0325
[RUSTSEC-2026-0326]: https://rustsec.org/advisories/RUSTSEC-2026-0326
[RUSTSEC-2026-0327]: https://rustsec.org/advisories/RUSTSEC-2026-0327

### Fixed

- **`impact` reaches what a schema change would break.** The verb walks a fixed
  set of incoming structural edges, and a table's and a column's were not in it:
  `READS`, `WRITES` and `USES_COLUMN` were in the graph and invisible to the
  question they answer, so `impact` on a column reported nothing affected while
  the statements that read it sat one edge away. The same under-reporting
  `USES_TYPE` was added to fix, for the facts the `xml` plugin's MyBatis reader
  writes. Containment stays out: `HAS_COLUMN`, like `CONTAINS`, says a thing is
  part of another, not that it breaks with it.

## [2.17.0] - 2026-09-30

### Security

- **The plugin sandbox is upgraded to wasmtime 48.0.3**, which closes three
  advisories published after 2.16.0 shipped: fuel accounting dropped by
  `call_ref` and exception `catch`, letting a guest amplify its fuel budget
  exponentially ([RUSTSEC-2026-0315]); dynamic record lifting allocating past
  the hostcall fuel limit ([RUSTSEC-2026-0316]); and a guest panicking the host
  through a filesystem datetime overflow ([RUSTSEC-2026-0314]). drsg runs
  third-party parser plugins inside that sandbox, so these are the class of bug
  it exists to contain. **2.16.0 and every earlier release that shipped the wasm
  plugin host are affected** — there is no patched 47.x, so upgrading is the
  only remedy. This raises the minimum supported Rust to 1.95, which wasmtime 48
  declares.

[RUSTSEC-2026-0314]: https://rustsec.org/advisories/RUSTSEC-2026-0314
[RUSTSEC-2026-0315]: https://rustsec.org/advisories/RUSTSEC-2026-0315
[RUSTSEC-2026-0316]: https://rustsec.org/advisories/RUSTSEC-2026-0316

### Added

- **`describe_plane` reports the key prefixes a plane holds.** A node's external
  key is prefixed, and the prefix is the part no convention reveals: a TypeScript
  key is prefixed with the `name` declared in the nearest `package.json`, not the
  repository directory, so a lookup by directory name finds nothing. Each
  producer's observed prefixes now appear under `key_prefixes`, keyed by
  `_generated_by` — derived by scanning rather than declared, so they cannot
  drift from the keys actually stored. A key with no separator contributes none,
  which keeps the set bounded by the packages a producer parsed rather than by
  node count.

### Fixed

- **A miss names the prefixes it did not match.** A lookup that guessed the
  prefix wrong was indistinguishable from one that named nothing at all: both
  answered "no symbol matches". It now lists the prefixes the plane does hold,
  says which guess was not among them, and points at the fragment that would
  have matched — a name matches any *fragment* of a key, so `src/App` finds
  `highway_bridge-3.0/src/App` without knowing the prefix. Reads keys only,
  rather than computing the whole catalog, so a mistyped name pays no more than
  it must (issue #40).

## [2.16.0] - 2026-09-29

### Added

- **A plane says when the installed plugins no longer match what built it.**
  Installing a plugin does not reach back into a graph that already exists: the
  files it claims stay unparsed, and `context` or `search` answer as if they were
  not there. A watched repository folds one commit at a time, so the tree ends up
  half parsed one way and half the other, and nothing said so — an empty answer
  looked exactly like an absent symbol. Every fold now records the plugin set it
  ran under in `built_by`, on the full identity `name@version+build` so a plugin
  upgraded in place counts too, and a plane whose installed set has since changed
  carries a `plugin_drift` note naming both directions. The note rides on every
  answer, including "no symbol matches" — the one case where the absence is
  ambiguous and the warning matters most. It is a warning only: nothing refuses
  to answer, and no fold is forced.
- **`rebuild`, an MCP tool that re-reads the whole tree a plane was built from**,
  so an agent can act on the warning it just read without leaving the session.
  `drsg init --rebuild` is the human twin. Both restore the plane's sync point
  afterwards, so the watch resumes where it left off.

## [2.15.2] - 2026-09-28

### Fixed

- **A rebuild reads the tree before it drops what it will replace.** `serve
  watch --force` dropped the plane, re-created it carrying the `rebuilding_since`
  marker, and only then routed the tree through the plugins. A parser that failed
  — or trapped, which a wasm plugin can do on input its author never saw — left
  the repository with an empty graph and a marker nothing cleared, and every
  later restart took the "server died, restart it" path and declined to refill.
  The graph `context`, `impact`, `trace` and `search` answer from was simply
  gone. The tree is now routed first and the plane is dropped only once the new
  facts exist, so a rebuild that cannot finish leaves the graph it has standing.

## [2.15.1] - 2026-09-27

### Fixed

- **An IPv6-literal URL is refused as the address it is.** `Url` brackets the
  literal, and `[::1]` is not something `to_socket_addrs` can take, so the
  address guard reported `cannot resolve [::1]` — a DNS story about an address
  that needs no DNS. It now says `refusing to connect to ::1: loopback`.
- **`::1` is loopback, not the v4-compatible spelling of `0.0.0.1`.** The
  embedded-v4 test claimed every `::`-prefixed address with a low bit set, which
  swept up IPv6 loopback and had the guard call it "an unspecified address". Both
  were refused before and after; this is the reason being right.

### Changed

- **ureq 3**, pinned exactly rather than as a caret range: the address guard
  hooks ureq's `Resolver`, which lives under the `unversioned` module that ureq
  documents as exempt from semver and breakable in a minor release, so a bump is
  a decision to re-read the guard. One user-visible consequence: ureq 3 tunnels
  every HTTP-proxy request with `CONNECT`, where ureq 2 sent absolute-form for a
  plain-`http://` destination. Both leave resolution to the proxy; a proxy that
  refuses `CONNECT` to port 80 is the case that changes, and no drsg path but
  `digest http://…` can reach it.

## [2.15.0] - 2026-09-27

A plane said `synced: commit <sha>` while holding symbols from files that commit
never contained. The label now tells the truth, and two flags let a graph be
narrowed to the code it is supposed to describe.

### Added

- **`--tracked-only`** reads only what git tracks, on `digest`, `init` and `serve
  watch`, with `[digest] tracked_only` beside it (#38). An ignore file says what a
  project considers derived; the index says what it has taken responsibility for,
  and a generated report sitting untracked is neither. It filters *paths, not
  content*: a tracked file with uncommitted edits is still read as it stands,
  which is one `git ls-files` rather than a `git cat-file` per file. Outside a
  repository it refuses rather than quietly reading nothing.
- **`--code-only`** reads only files an installed handler claims, leaving
  Markdown, PDFs, spreadsheets and unclaimed extensions to the document reader
  they belong to (#38). A graph of a codebase, not of the documents beside it —
  and since prose is what needs a model, a code-only digest of a pure-code tree
  makes no chat call at all. What a handler may `read` is deliberately not
  narrowed: it pulls the files *around* the one it was given.
- **A plane records how far its tree had drifted** from the commit it names, and
  every answer read from it says so: `synced: commit ec31cac58593 — parsed from a
  working tree with 2 modified and 1 untracked, so the graph holds what that
  commit does not`. Silence means the tree matched, or that the plane predates
  this being recorded — the same silence on purpose, because claiming "clean" for
  a plane that never measured would be the overclaim this replaced.

### Fixed

- **`synced_commit` stopped overclaiming.** It is stamped from `git_head` at fold
  time and was described as "commit the plane reflects", while the walk reads the
  working tree and never consults git's index. It now says what it always meant:
  the commit HEAD was at when the facts were parsed.
- **A digest no longer reads drsg's own output back in.** The rolling log
  (`DRSG_LOG_DIR`, default `./logs`) was read *as prose*, so a pure-code tree
  demanded a chat provider it had no use for; the `*.drsg` store directory and
  its sidecars were ingested beside it. All three join the walk's floor, which no
  longer takes an `init` to arrange.

## [2.14.0] - 2026-09-26

A repository whose `.dockerignore` excluded `src/` digested to nine nodes instead
of six thousand, and said nothing. Both halves of that are fixed.

### Added

- **`--ignore-files` and `[digest] ignore_files`** choose which of a project's
  ignore files decide what is source — `gitignore`, `dockerignore`, both, or
  neither. Accepted by `digest`, `init` and `serve watch`; the flag beats the
  file. An unknown name is refused rather than skipped, because a typo that
  silently changed which files were read is the failure this setting exists to
  end. `IgnorePolicy` had these knobs already and nothing could reach them.
- **An ingest says what it read.** `-v` gives the count and the share of the
  tree, `-vv` adds each withheld file and the rule that withheld it, `-vvv`
  names every file and the handler that read it. The denominator is what drsg's
  own floor left, not every file on disk: a `node_modules/` of 80,000 files
  would make the percentage meaningless.
- **A mostly-withheld tree warns without being asked**, naming the ignore file
  responsible and the flag that overrides it — the case where nobody thought to
  pass a flag is exactly the case that needed one. `serve watch` reports the
  same through its log, since its output is a log rather than a terminal.

### Changed

- **`.dockerignore` is no longer read by default.** It answers a different
  question — what belongs in the build context sent to the Docker daemon — and
  the answers routinely differ: a front end built in CI and shipped as `dist/`
  is right to keep `src/` out of its image, and that says nothing about whether
  the source is derived. It was also the *least* overridable file in the
  repository: the `ignore` crate gives a custom ignore file precedence over
  every other, so a `.dockerignore` outranked the `.gitignore` beside it and no
  `.ignore` or `!src` could win it back. A tree that digested almost nothing
  under 2.13.0 will digest all of it here; `--ignore-files
  gitignore,dockerignore` restores the old reading.

## [2.13.0] - 2026-09-25

Outbound requests honour an HTTP or SOCKS proxy, and the graph cache no longer
answers across a boundary it cannot see.

### Added

- **A proxy carries the requests drsg makes on your behalf** (#37): `plugin
  install`, `update`, `digest <url>`, the LLM provider and a replica's snapshot
  fetch read `ALL_PROXY`, `HTTPS_PROXY` and `HTTP_PROXY` (each also lowercase),
  or a new `[network]` section — `proxy` and a `NO_PROXY`-style `no_proxy` — for
  a daemon with no shell environment to read them from. An environment variable
  that is set wins over the file, so one command can be run through a different
  proxy, or through none with an empty value. `socks5://` works as well as
  `http://`. `NO_PROXY` is implemented here: ureq has none of its own.

### Changed

- **A proxied request is not an address-guarded one.** The two are alternatives,
  not layers: ureq connects to the proxy and hands the destination over as a
  name, so the guard's resolver is asked for the proxy's address — usually
  loopback, which it refuses — and never sees the address it exists to judge. A
  URL the operator named is therefore proxied; a URL a *caller* named
  (`/digest/fetch`, `plugin.install` over RPC, which is only `Access::Write`
  gated) stays direct and guarded. `serve` says which is in force at startup
  when a proxy is configured.
- **Loopback is never proxied**, whatever is configured — a local model server
  (the `ollama` preset is `http://localhost:11434/v1`) is not what an operator
  who set `https_proxy` for the outside world meant to redirect.
- A transport failure through a proxy names the proxy, with the password
  redacted, instead of reporting only "the request failed".

### Fixed

- **The graph cache could answer for the wrong plane.** It keyed decoded records
  and adjacency by bare id; ids are global but a read is plane-scoped, so
  `planeB.query().seek_ids([id_in_A])` found a node the uncached path does not.
  The plane is now part of every key rather than checked on hit.
- **Recording a query flushed the whole cache.** `record_query` went through
  `with_write`, which bumps the commit sequence every L2 entry is stamped with,
  so every query run through the web or MCP layers — which record it — emptied
  the cache before the next query could use it.
- **Restore and replication invalidate the cache they cannot stamp past.** Both
  land a foreign commit sequence rather than bumping one, so new data can arrive
  under a sequence this database has already stamped entries with; restore
  served an empty adjacency for a node it had just loaded edges for. A
  generation, bumped before the clear, also closes the window where a reader
  that opened before the commit could repopulate it afterwards.
- **`AS OF` keyword search is historical.** A time-travelling handle answered
  keyword and hybrid-keyword searches from the live BM25 registry, which only
  describes the latest commit, so it returned nodes created, deleted or
  relabelled after the pinned point and scored them on their latest text.
  `KeywordRegistry::search_snapshot` builds a throwaway index from the pinned
  transaction, the shape the vector path already had.
- **`drsg plugin install` refused a name its own resolver could not answer.**
  The address precheck resolved locally before any packet moved, so on a network
  where the name resolves to `::` — which is why a proxy is there — the fetch was
  refused before it could be tried. A proxied request is not resolved locally.
- The proxy policy resolves in a build without the `digest` feature, where the
  LLM crate is not linked.

## [2.12.0] - 2026-09-23

Three core groups from the audit branch, and the storage migration they made
necessary. A store written by this release is not readable by an older binary:
new SSTs carry the `DRS2` magic. Old files still open here.

### Added

- **`drsg upgrade` rewrites storage runs that predate block checksums.** A v1
  run is rewritten as v2 by the next compaction, but compaction only fires
  above four runs, so a database that settles below that kept unchecked blocks
  for good — and whether a store ever gained checksums depended on how often it
  happened to be written to. `drsg check` now counts such runs and names the
  command; the rewrite re-encodes each run's entries as they stand, so unlike a
  compaction nothing is reclaimed and no version becomes unreachable. It takes
  the writer slot and is safe to interrupt.
- **A failed flush or compaction is visible.** `Database::last_maintenance_error`
  surfaces through `drsg check`, `drsg stats` and `db.stats`. A disk that keeps
  taking the WAL but refuses new files used to look healthy from every operator
  surface while the WAL and memtable grew without bound.

### Changed

- **A native commit returns `Ok` once its batch is durable.** It used to return
  whatever the flush it triggered said, so a failed flush became an `Err` after
  the write was fsynced and visible — and the layer above reads `Err` as
  "nothing landed", skipping its index and keyword events. Maintenance failures
  are now reported through the slot above rather than through the commit.
- **An SST block carries a CRC-32.** The footer magic names the format: `DRSS`
  (v1, no checksums, read-only) and `DRS2` (v2, written now).

### Fixed

- **Deleting the HNSW entry node no longer panics the next search.** The
  replacement was an arbitrary live node while `top_layer` was left alone, and
  most nodes carry only layer 0 — so the next query indexed past the
  replacement's layers. Any delete that removed the entry vector was enough.
- **A corrupted SST block is an error, not an answer.** Every scan loop ended
  quietly at a bad entry: a point read said "absent", a range stopped short,
  and a compaction dropped the rest of the block for good.
- **Durable commits could vanish.** An fsync error between the WAL truncate and
  the seek left the cursor at the old end, so later batches landed behind a
  zero-filled hole that replay reads as a torn tail — and the next open cut the
  file at zero.
- **A crash during compaction could resurrect a deleted key.** The unlinks of
  replaced runs were not fsynced into the directory, so an old run holding a
  put could come back without the newer run holding its tombstone.
- **A JSON integer past `i64` is refused** instead of wrapping to a negative
  number. Every JSON write path went through that cast. A nested map with a
  `$`-prefixed key now round-trips.
- **`LIMIT` can stop a variable-length walk.** The expansion materialized every
  walk from a start row before the next step saw the first, so a deep bound ran
  to exhaustion before `LIMIT` — or the query deadline — could stop it.
- **A bulk-loaded external key must be free in the plane.** The loader checked
  uniqueness only within its batch, so a key already owned by a live node left
  two claimants.
- **The hybrid graph channel scores only the queried label**, and a NaN or
  infinite score no longer poisons a channel's normalization.
- **A frontier is ranked as a set**, so `k` counts distinct nodes and each keeps
  the walk that reached it.

### Performance

- Compaction streams a k-way merge instead of loading every run; its peak was
  the sum of the runs, which on an embedding-heavy store is the store itself.
- A flush writes its SST under the store read lock, so readers are no longer
  stalled through the write, fsync and rename.
- The change feed caps itself at the source: a million-node load buffers 256
  change tuples, not a million.
- HNSW search borrows a per-thread scratch instead of zeroing one per query,
  and the scratch is capped so a worker that once searched a large index does
  not pin it for good.

## [2.11.1] - 2026-09-22

### Added

- **`drsg.toml` carries the last knobs that were environment-only.**
  `[plugins] deadline_secs` and `total_memory_mb` bound the plugin sandbox;
  `[server] allowed_hosts` lists the extra `Host` values `/mcp` answers at;
  `[server] mcp_tool_deadline_secs` bounds one MCP tool call. An environment
  variable already set still wins over the file, as for every other key.
- **`DRSG_PAGE_TOKEN=0` (`[server] page_token = false`) keeps the token out of
  the page** for a loopback bind behind a reverse proxy, where every client
  arrives looking like a local one.

### Fixed

- **A loopback bind behind a reverse proxy hands out neither the page token
  nor the local-UI fallback.** A proxy in front of the listener makes every
  client's peer address loopback, so 2.10.0's loopback rules would have given
  the token to the network. A forwarded request is no longer treated as local,
  and a server with no token refuses to start when `DRSG_ALLOWED_ORIGINS` names
  an origin off loopback — such an origin is only ever reached through a proxy.
- **The bind policy is checked when the config is read.** A `[server] addr` off
  loopback with no token now fails before the database is opened, rather than
  after a follower has wiped its directory or a watch has started folding a
  tree. The compose file marks `DRSG_TOKEN` as required, so `docker compose up`
  stops with the reason instead of starting a container that exits unseen.
- **The auth throttle counts only guesses**, and tells a client behind a proxy
  when it has been locked out.
- **An environment deadline wins over the file.** `mcp_tool_deadline_secs` in
  `drsg.toml` overrode `DRSG_MCP_TOOL_DEADLINE_SECS` even when the variable was
  set.
- **The docs say `retain_commits = 0` is unbounded.** The Web UI chapter told
  readers to unset the key for unbounded history, but an absent key is the
  default of 20. The OpenRPC schema now lists the `retain_commits` field
  `db.stats` answers.

## [2.11.0] - 2026-09-21

### Changed

- **What destroys is confirmed over MCP.** `drop_plane` already required
  `confirm: true`; now a `cypher` statement carrying `DELETE` or `REMOVE`, and
  `digest` with `apply: true`, do too, and refuse without it in the same
  words. `CREATE`, `MERGE`, `SET` and the bulk writers stay ungated — asking
  twice for the everyday additive work would only teach clients to pass
  `confirm` reflexively. A client that deletes over MCP must now pass the flag.
- **A tool call ends.** Each call carries one deadline covering the wait for a
  slot and the run — five minutes by default, `DRSG_MCP_TOOL_DEADLINE_SECS`
  to change it (`0` removes it). Past it the call answers with a tool error
  saying whether the server was busy or the body was slow, rather than never
  returning.

### Fixed

- **The stdio relay takes a credential only from the repository's own file.**
  `drsg-mcp` walked every ancestor of the working directory for a `.mcp.json`
  and forwarded the `Authorization` header it found, so a host launched in a
  checkout under a shared or hostile parent directory handed its session —
  token included — to whatever server that file named. The walk now ends at
  the first directory holding a `.git`, and on Unix a file is read only when
  the effective user owns it and no one else may write it.
- **A tool parameter cannot name a URL or a key variable.** `cypher`,
  `hybrid`, `ask` and `digest` handed a request-supplied provider straight to
  the provider builder, which accepts a raw base URL — so an authenticated
  caller could make the server POST, with a key from its own environment, to
  any host it could reach, and choose which variable the key was read from.
  A provider named over MCP must now be a preset or the one the operator
  configured, as it already had to be over JSON-RPC.
- **The confirm gate reads a backtick name the way the parser does.** It
  honoured a backslash inside backticks where the grammar does not, so
  `` MATCH (n:`a\`) DETACH DELETE n `` parsed and ran unconfirmed.
- **The stdio binary keeps the retention the server does.** `drsg-mcp`
  opened its database with none, so a store driven only through a stdio host
  kept every version it ever wrote. It now opens with the served default, or
  `DRSG_RETAIN_COMMITS` (`0` keeps everything); a value that is not a number
  is a start-up error rather than a silent default.
- **The redb-only build compiles again.**

## [2.10.0] - 2026-09-21

### Changed

- **A server without a token refuses to listen anywhere but loopback.** It
  starts with an error naming `DRSG_TOKEN` and the `--addr` way back, rather
  than coming up open with an `Origin` header — which any client can type — as
  its only credential. A `drsg serve --addr 0.0.0.0:7700` that worked before
  now needs a token; the container image already sets one.
- **The page carries the token only to a local human.** `GET /` is
  unauthenticated by necessity — it is how a browser fetches the page that
  will authenticate — and it spliced the token into the HTML for every caller.
  The token is now written only when the bind *and* the peer are both
  loopback, and as a `<meta>` element rather than an inline script, so every
  response can carry `script-src 'self'`. A dashboard reached over the network
  asks for the token once and keeps it for the tab.
- **`/mcp` answers at the names the operator lists.** Besides loopback and the
  bind address, the endpoint answers only at hostnames given through
  `[server] allowed_hosts` / `DRSG_ALLOWED_HOSTS`, and only once a token gates
  it — the transport's DNS-rebinding guard, which an empty list switches off.
- **A JSON-RPC batch holds at most 64 requests**, refused whole before any
  item runs, rather than as many as fit in a 64 MiB body.

### Fixed

- **A wrong token is counted.** `/rpc` answered every wrong bearer at wire
  speed, so the only thing between a client and the token was the token. Past
  five failures a peer serves a wait that doubles to five minutes, applied as
  middleware over the whole router so no route can forget it; a request with
  no bearer is not a guess, so the zero-config local UI is untouched.
- **A NAT64 spelling of a protected address is that address.** The fetch guard
  read the v4 address inside v4-mapped, v4-compatible, 6to4 and Teredo forms
  but not inside the well-known prefix `64:ff9b::/96`, so on a host with a
  NAT64 gateway `64:ff9b::a9fe:a9fe` reached the cloud metadata service and
  `64:ff9b::7f00:1` reached loopback while reading as ordinary public v6. The
  local-use block `64:ff9b:1::/48` is refused whole.
- **A follower's replication queue has a size**, and its pull-time backlog is
  forwarded by one bounded task rather than a task per batch.
- **`plane.find` stops.** The node scan walks a page at a time to a cap, and
  the edge pass now caps the nodes it visits as well as the edges it examines
  — a needle matching no edge on a plane of mostly leaves looked up the
  neighbours of every node, once per keystroke.
- **A snapshot is spooled before it is streamed** and an export walks ids, so
  neither holds the whole database in memory while a client reads slowly.

### Added

- **Every method in the contract names its access tier**, and every parameter
  says whether it is required. The six generated SDK clients carry the tiers.
- **The dashboard is told how deep the history it offers goes**, so it does
  not offer time-travel past what the server retains.

## [2.9.2] - 2026-09-19

### Fixed

- **A model cannot stamp provenance, plant a vector, or key a node on
  nothing.** Extraction inserted whatever property name the model emitted, and
  the provenance stamp afterwards overwrote only `_source`, `_model` and
  `_run` — so a model-emitted `_generated_by` survived. That property is what
  marks a node as a parser's, and a parser-owned node is one the next watch
  fold may delete or rewrite, so a document written to steer the model could
  hand the graph's ownership to whoever wrote the document. The same path
  accepted `embedding` and a vector under any other name (a steered
  similarity search), empty property names, and entity keys that were blank or
  a paragraph long. A model-emitted name must now be trimmed, non-empty,
  printable and bounded, and the properties only the pipeline may write are
  refused at both stages that accept them; the report counts what was dropped,
  because a model does not produce these — a document written to steer one
  does.
- **`ask` runs only the grammar it teaches, and never an unbounded plan.** The
  safety cap was appended only when the plan declared none, so a model that
  wrote `{"Limit": 1000000000000}` ran unbounded on a call reachable from the
  Read tier; and the plan itself was checked against nothing, though the model
  could already emit the variants the prompt forbids. Plans are now checked
  against an allowlist of the taught grammar and every limit — the model's,
  the caller's, a projection's — is clamped to a ceiling. `limit: 0` means
  that ceiling rather than no ceiling.
- **The chat provider keeps its key out of its errors.** A transport error was
  returned as the HTTP client renders it, leading with the URL — which is
  where a key ends up when an operator was told to supply a base URL
  (`…/v1?api-key=…`, `https://token@host/`). Errors now describe the fault,
  and the messages that must name an endpoint name it with the query string
  and userinfo cut. `Retry-After` is honoured up to sixty seconds instead of
  being clamped to the backoff schedule, and a dead provider stops a run after
  one refusal rather than after every chunk has made its own doomed request.
- **A plugin cannot read what the host would not list.** The sandbox also
  gains a clock, a shared memory budget across plugin instances, and a version
  that is part of a build's identity, so a rebuilt plugin is not mistaken for
  the one it replaced.
- **A commit fold is one transaction.** The fold committed its delete-and-load,
  then opened a second transaction to re-attach the incoming edges of replaced
  nodes and carry their vectors across. Between the two commits a reader saw a
  replaced symbol with no embedding and no links into it, and a crash left it
  that way for good.
- **The ingest ledger is written under the plane's write lock**, and a digest
  reply that is not JSON is asked for once more before the run is abandoned.

### Changed

- **The embedding pass walks the plane by id** instead of cloning every
  record, and embeds a batch at a time.

## [2.9.1] - 2026-09-18

### Fixed

- **The updater runs the installer the release shipped.** `drsg update` pinned
  the archive but fetched `scripts/install.sh` from `master`, whatever release
  it had resolved — so anyone able to move the branch replaced the verifier on
  every machine that upgraded, with no release ever being cut. The command is
  now built from the tag the check resolved, and every interpolated value is
  quoted once.
- **An unverified archive is refused.** Both installers checked the SHA-256
  only if the sidecar happened to download, and said nothing when it did not:
  a truncated download or a swapped asset installed as readily as a good one.
  A missing, malformed or mismatching sidecar — or having no `sha256sum` to
  check with — is now a hard failure, with `--insecure-skip-checksum`
  (`DRSG_INSECURE_SKIP_CHECKSUM=1`) as the documented way past for a mirror
  that publishes none.
- **A committed agent config never receives the token.** `drsg init` wrote the
  live bearer token in clear into `.cursor/mcp.json`, `.opencode.json` and
  `.gemini/settings.json`; the last two pre-exist `init`, which is what made it
  write to them, so they are as likely as not already committed. OpenCode and
  Gemini now reference `DRSG_TOKEN` as the Codex entry already did, and Cursor —
  a desktop app with no shell environment to read — joins `.mcp.json` in the
  gitignore block `init` maintains. `drsg.example.toml` no longer ships a live
  `token = "change-me"`.
- **Every action a workflow runs is named by the commit it is.** The workflows
  named third-party actions by floating tags, so every run of ci, docs and
  release fetched whatever those tags pointed at that day and ran it with the
  repository's checkout — and, for release, its registry and release
  credentials. Each `uses:` now names a full commit SHA.
- **The shell guard reads a redirect, not an arrow.** Any command containing
  `>` or `<<` counted as a write, so `rg '>' src`, `grep "a -> b"` and
  `rg needle 2>/dev/null` went straight past the guard. The check now runs on
  the command with its quoted text removed and numbered-fd redirects dropped.
- **A hook is ours by its exact path.** `init` claimed any hook whose command
  merely ended in our script's name — a wrapper, or a team's own copy at
  another path — and repointed it at ours. It now matches only a bare command
  whose last component is exactly the name, and writes a quoted command where
  the project path holds whitespace.
- **The usage report keeps its watermark where only its user can write.** It
  lived at a predictable name in `/tmp`, so a file or symlink planted there was
  followed on write and believed on read. It now lives under
  `$XDG_RUNTIME_DIR/drsg` (falling back to `~/.cache/drsg`, both 0700), written
  through an `O_EXCL` 0600 temporary and renamed into place.
- **`init` waits for its own child, and signals only a drsg.** Any accepted
  connection counted as "up", so a process that won the picked port had `init`
  reporting success — pid, URL, agent configs — against a stranger; and
  `stop_server` sent SIGTERM to whatever pid `/health` named. The wait now
  requires an answer carrying the child's pid, a lost port race is retried on
  another, and a pid whose `/proc` cmdline is not a drsg binary is refused.
- **Every command opens the database with the retention the server would.**
  `[server] retain_commits` was applied only by `serve`, so a store driven from
  the command line kept every version ever written and carried them through
  every compaction.
- **The committed bun lock is the dependency set.** Four `bun install` sites ran
  unfrozen, dating from a bun canary lockfile a stable bun could not parse, so
  each build re-resolved and a republished package could change what shipped.
  Stable bun reads `lockfileVersion 2` now, and every site is frozen.

### Added

- **The Zig client has a CI job**, mirroring `sdk-c`, and `just gate-sdk` runs
  it — a schema change that broke it previously went red nowhere.
- **The usage hook's watermark handling is pinned by a unittest module**, run by
  a `hooks` CI job and `just gate-hooks`.

## [2.9.0] - 2026-09-17

### Changed

- **What the subset leaves out is refused, not silently misread.** A
  relationship variable (`-[r:T]->`) was dropped without a word; an inline
  property predicate in a `MATCH` node, a second `MATCH`, `OPTIONAL MATCH`,
  `WITH`, `UNWIND`, `UNION`, a pattern with several paths, list and map
  literals in expression position, and the `%` and `^` operators now each
  fail with an error naming the rewrite. A query that appeared to work on
  2.8 may fail here, which is the point: it was not doing what it read as.
- **A missing property satisfies neither a predicate nor its negation.**
  Core's evaluator is two-valued, so `d.year <> 2020`, `NOT d.year = 2020`
  and `NOT d.year IN [2020]` all kept nodes with no `year` at all, where
  openCypher's null would have dropped them, and `d.x = null` matched
  exactly those nodes. The compiler now wraps such a predicate's nullable
  leaves in `IS NOT NULL` guards without teaching the executor three-valued
  logic; `IS [NOT] NULL` remains the way to ask about absence. Under `NOT`,
  a compound predicate whose false branch would have absorbed the null is
  stricter than openCypher, and the query-language pages say so.

### Fixed

- **A deep expression no longer aborts the process.** The nesting guard
  covered the grammar's recursion but not its loops, so a hundred thousand
  `OR`s — or ten thousand parentheses — overflowed the handler stack, and a
  stack overflow is not a panic any `catch_panic` layer contains: one
  oversized query on `POST /cypher`, `plane.cypher` or the MCP `cypher` tool
  took the whole server down. Each operator of a chain is now charged to the
  same 64-level budget, and `x IN [...]` expands to a balanced tree rather
  than a left-deep one.
- **The completer reads characters, not bytes.** Its tokenizer walked bytes
  and cast each to a `char`, so a multi-byte identifier such as `函数`
  panicked on a slice that stopped inside a code point. It now walks
  `char_indices`: identifiers are Unicode words or backticked names, a
  string skips the character after every backslash, and a number carries its
  fraction and exponent.
- **A predicate over `hops()` or `score()` is evaluated at the end of the
  walk.** Both were pushed down to the source, where `hops()` is 0 and a
  `MATCH` row has no score yet, so `WHERE hops() = 2` on a `*1..3` walk
  silently returned nothing although the plan looked right. Such a conjunct
  now goes on the last slot — including one that mixes a channel with an
  earlier variable, which no AND-split separates.
- **A write mutates each matched node once, and `MERGE` sees its own
  statement's edges.** A pattern reaching the same node twice deleted it
  twice; `ensure_edge` consulted only the committed store, so a `MERGE` that
  walked one edge twice created it twice and stopped being idempotent.
- **The lexical surface matches what the docs promised.** `ORDER BY
  count(*)` was a syntax error; strings had no escapes, so a value
  containing a quote ended the literal early; identifiers were ASCII-only
  with no backtick escape; numbers took no exponent and an int past `i64`
  fell into a float; function names were case-sensitive where keywords were
  not; `shortest_path(weight: "Cost")` looked up `cost`; and
  `iterations`/`max_levels` truncated through an `as u32`.
- **An error names its cause.** Every mistyped write was reported as "near
  `CREATE …`", because the statement grammar was a blind `alt` and nom
  returns the *last* alternative's error. Statements now dispatch on their
  leading keyword and each clause commits on its own, so a fault is reported
  where it is.

## [2.8.1] - 2026-09-17

### Fixed

- **A file the source tree does not hold is never read.** `snippet` joined
  whatever it was handed onto the tree's root and read the result, so
  `../etc/passwd:1`, an absolute path, and a node whose `file` property pointed
  elsewhere all read outside the tree; a symlink planted in a checkout took
  `grep`'s walk out of it as well. On a served `/mcp` that was an
  arbitrary-file-read for any authenticated agent. Every read now passes
  through one helper that refuses a path by shape first and then by where it
  resolves, which is what catches a symlink.
- **A plane's `synced_root` no longer chooses what a server opens.** The tree a
  plane records is data — `plane.set_props` writes it — so it is honoured as a
  tree of its own only where the process already runs as the user whose files
  these are: the stdio server and the CLI. On a shared server it is read only
  when it lies inside the tree the operator attached, and with nothing attached
  nothing is read.
- **The SDK end-to-end teardown waits for its server.** The C and Zig suites
  killed `drsg serve` and removed its directory in the same breath; the server
  was still writing its index sidecars, and a file landing in a directory just
  emptied failed the removal with `ENOTEMPTY` after every check had passed.

## [2.8.0] - 2026-09-16

### Added

- **`recall` reads code as it was at any commit.** The `_git` plane named
  every commit a repository ever had, but reading one meant leaving the tools
  for `git show`. `recall` (MCP, and `drsg recall`) reads a file, a line range,
  a directory or a symbol at a revision named by sha, branch, tag, date or
  `HEAD~n`; `pattern` searches that tree and `vs` diffs a name from an older
  revision. A symbol is found by parsing its file as it was, so the answer is
  the declaration then rather than today's line numbers — and a symbol that
  commit did not declare is reported as absent, with what the file did hold.
- **Agents are pointed at it.** The server's instructions name `recall` for
  old code, and the shell hook redirects `git show <rev>:<path>`, `git grep`
  and `git log -p` / `-L` to it.

### Fixed

- **The TLS stack takes its patch.** rustls moves to 0.23.45 for
  RUSTSEC-2026-0285: TLS 1.3 handshake messages were accepted at the wrong
  encryption level when they followed a key-changing message in the same
  record. The transcript stays authenticated, so a handshake could not be
  altered or completed by it; what was missing is the `unexpected_message`
  alert RFC 8446 §5.1 requires.

## [2.7.1] - 2026-09-10

### Fixed

- **`grep` says why a regex-shaped pattern found nothing.** A regex sent to a
  literal search returned `no matches`, the same words a real absence returns.
  A miss whose pattern carries `|`, `.*`, `.+`, `\d`, `\w`, `\s` or `\b` now
  names the construct and points at `regex: true`.

- **The wasm plugin sandbox takes its patch.** wasmtime moves to 47.0.4 for
  RUSTSEC-2026-0269, a filesystem sandbox escape on a trailing slash, and
  RUSTSEC-2026-0268.

## [2.7.0] - 2026-09-06

### Added

- **A build manifest is read as a declaration, not as prose.**
  `package.json`, `go.mod`, `requirements.txt`, `pyproject.toml`, `pom.xml`
  and `build.gradle` were claimed by no plugin, so they fell to the document
  reader — which needs a model, making a facts-only digest of any JS
  repository, or a Rust one with a stray `go.mod`, a hard stop rather than a
  thin answer. The host now routes those **filenames** (not extensions:
  `package.json` is not "every `.json`") to whatever is installed under the
  reserved name `deps`, the same statement it already makes sending a `.git`
  directory to `git`. A list of such names, so a repository running two build
  systems is read by both, and with nothing installed under a name its files
  are read exactly as before. The reader itself ships as
  [`deps@1.0.0`](https://github.com/wangyingsm/dr-strange-extension) and
  keys each dependency the way the code plugins key an import, so a declared
  `express` and an imported `express` are one node carrying the version the
  manifest states. Two plugins naming the same `External` key now agree
  rather than collide — that meeting is the point.
- **Every fact says which build produced it, and the plane keeps the
  account.** `_generated_by` carried the fact-format version alone, so two
  releases of one parser stamped identically and a plane with no `Channel`
  nodes might hold no channels or might predate the parser that could see
  them. The artifact's hash rides on the mark now — `rust@2+c966ecba` — ten
  bytes inside a property every node already has. The last whole-tree
  ingest's account lands on the plane beside `synced_commit`: which handlers
  ran and what each contributed, what was skipped, which extensions no plugin
  claims. Answers say so when that ingest was incomplete, and stay silent
  when it was not.
- **`drsg snippet`, `drsg grep` and `drsg traverse`.** Three of the MCP
  surface's verbs had no subcommand: reading a symbol's source, searching the
  tree a plane was parsed from, and walking an edge could only be done by an
  agent talking MCP, never from a prompt or a script. They call the same
  functions the tools do. Both file-reading verbs find the tree the way the
  tools do — the plane records where it was parsed from — with `--root` for a
  plane that recorded none.
- **An edge's endpoints are links.** `CALLS · 1421 → 883` named two nodes as
  numbers you then had to find by hand. Each is now the link a property value
  already gets: click to centre it, hover for the key it stands for.

### Changed

- **`impact` on a type sees what is typed by it.** The walk's edge set
  predated the plugins having anything to say about types: on a type it saw
  only `INSTANTIATES` — who built one — and never who takes it as a
  parameter, returns it, or holds it in a field, then reported that fraction
  with an exact count and no sign the question had been answered narrowly.
  `USES_TYPE` joins the walk, and the MCP tool description says so.
- **`snippet` reads a symbol to its end.** It read a fixed forty lines after
  a declaration's first, because that was all a node recorded. The parsers
  write `end_line` now, so it reads the declaration and stops — 22,738 lines
  against 52,920 across `dr-strange-core`'s 1,323 symbols, with a median
  symbol of eight. An explicit `lines` still wins, the cap still bounds a
  long one, and a plane whose parser recorded no extent behaves exactly as
  before.
- **`context` says a call departs from waiting.** Plugins have written `go`,
  `scheduled`, `unawaited`, `spawned` and `blocking` onto the CALLS edge for
  releases, and the verb an agent actually reads dropped all of it. `[spawned]`
  rides beside `call@25` now. Silent for an ordinary call: waiting is the
  expectation.
- **`context` says how its calls were bound.** Every plugin stamps
  `_confidence` on the edges it resolves and no verb had ever shown it, so an
  edge bound by matching a bare name against what is in scope read exactly
  like one the source spelled out as a path — about a fifth are the former.
  Said once in the footer, not as a marker on a quarter of the lines.

### Fixed

- **`snippet` no longer invites you into the next declaration.** It printed
  "… continues; reads on" after reading a symbol to its end — true of the
  file and false of the symbol. It says that now only when the answer really
  was cut short.
- **The hover box names a node the focus fade blanked.** Selecting a node
  drops the labels of the far graph, and the drawer read the label the
  reducer left behind — so hovering a node three hops out drew an empty chip,
  when that is exactly the node whose name is being asked for. It reads the
  key back off the graph, as the selection ring already read its colour.

## [2.6.0] - 2026-09-05

### Added

- **The Query box asks the plane what could come next.** `POST
  /cypher/complete` takes however much of a query is written and answers
  with what may follow it — the clause, the labels this plane actually
  holds, the edge types that leave them, the properties a bound variable
  has, `key(n)` where a projection or a sort wants a value — each carrying
  the count that ranked it, read from the plane's own catalog rather than
  from a fixed grammar. It completes from a cut anywhere: mid-word, mid
  `-[:TYPE]->`, inside a call's brackets, after `ORDER BY`. A property test
  asserts that every truncation of a real query can be finished from where
  it was cut. The Query page asks after a second's pause in typing, draws
  the best guess as ghost text after the caret (Tab takes it), and lists
  the candidates below with what each one is.
- **The database remembers the queries that ran, and offers them back.**
  A capped history — two hundred entries by default, newest first, the
  oldest dropped as a new one arrives — recording what *ran*, not what
  parsed: a query the plane rejected is not something to hand back as a
  thing you once did. Re-running the query you just ran restamps its entry
  rather than adding a second copy. `GET /cypher/history` lists them and
  `GET /cypher/history/{id}` reads one (both also answer to POST, which is
  how the browser UI authorizes itself). `drsg queries` prints the list and
  `drsg queries <id>` one query's text alone; `drsg cypher --history <id>`
  runs a saved one again, against the plane it was written for. The Query
  page shows the list beside the editor, a click away from the box. Queries
  an agent runs through the MCP `cypher` tool are recorded the same way.
- **A memory badge on the dashboard.** What the server process holds right
  now, with what the loaded plugins hold as its subtitle — read per
  platform (`/proc/self/status` on Linux, `proc_pidinfo` on macOS,
  `GetProcessMemoryInfo` on Windows) rather than on Linux alone.
- **`drsg init --rebuild`.** Drops the plane and folds the whole tree
  through the plugins as they are installed now — what to reach for after
  upgrading a language plugin, since the parser decides what a source file
  means and a resumed plane keeps whatever the old one made of the files it
  had already seen. It stops the server that holds the database (and the
  loaded copy of every plugin) and brings it back on the same address and
  token, so no agent's configuration changes underneath it. `GET /health`
  now reports the server's `pid`, which is how it knows what to stop.
- **The Query page reads its answer a page at a time.** Two hundred rows or
  nodes to a page, with the range and the total in the header and prev/next
  beside them. A pattern over a real codebase answered with four thousand
  functions each carrying its own source — eleven megabytes to ship, parse
  and lay out, for a table nobody scrolls to the end of.
- **A result table is read column by column.** Labels are coloured chips
  (deterministically, so a label is the same colour on every page), the
  embedding has its own column as a button saying how many dimensions it
  holds, and the remaining properties show in brief with an expand button
  that opens the whole map as syntax-highlighted JSON.

### Changed

- **An embedding is a marker everywhere, and a click away when wanted.**
  Nothing that reads a node drew its embedding, and yet every read shipped
  a thousand floats per node because `lean` was a flag the dashboard never
  set — one query came to 35.9 MB of which 34 were numbers nobody looked
  at. Every read now renders a vector as `$vector(N dims, omitted)`,
  including a *projected* one (`RETURN m.embedding` asks for one by name).
  The floats have one door left: `node.get` with `lean: false`, one node at
  a time, which is what the dashboard's own button now goes through.
  **Breaking**: `lean` defaults to true on `node.get`, `plane.neighbors`
  and `plane.cypher`. A caller that wants embeddings inline passes `lean:
  false`; the generated SDK clients need no change, since they carry no
  defaults of their own.
- **The Query page opens empty.** It used to open on a fixed statement that
  failed on most planes — a statement someone has to read and delete before
  they can start. The placeholder says the shape instead, and the examples
  below the box write a real one on request.

### Fixed

- **A node reached many ways comes back once.** A pattern reaches the same
  node through every path that lands on it, and the result table is keyed
  by node id: the repeat did not merely look untidy, it aborted the render,
  and the page sat on "Running…" with no result and no error for as long as
  you cared to wait.
- **Every row of a result table is one height.** Two cells laid their own
  contents out with `display: flex`, which takes a `td` out of table layout
  — the row stopped sizing them and their rule was drawn short of the rules
  either side. Rows also took their height from whatever the tallest cell
  happened to hold, none of those a whole number of pixels, so each row
  rounded differently from the one above; and a collapsed border between
  two rows is one edge shared by the cells either side, rasterized per cell,
  so a half-pixel edge rounded up in some columns and down in others.
- **The hint row keeps its line.** It was shown only while the box had
  focus, so it vanished on the way to the Run button and moved the button
  out from under the click.
- **A half-written hop is completed as written.** A lone `<` owes its
  hyphen and `<-` owes its bracket; a hop is finished from however much of
  it is on screen rather than only from a bare `-`.
- **The caret starts where the box does.** It was assumed to be at the end
  of the text, so the first completion after opening the page described a
  position nobody was at.

## [2.5.0] - 2026-09-04

### Added

- **`grep` can do what the shell `rg` could, and says where each hit lives.**
  The MCP verb takes `regex: true` (Rust syntax — alternation, anchors,
  classes), `path` to scope a search to a directory, a file or an extension,
  and `context: n` for surrounding lines, printed the way `rg -C` prints them
  (`-` on context lines, `--` between groups). Each hit is followed by the
  symbol it falls inside — `in crate::module::fn` — read off the plane parsed
  from that tree, so the next call is `context` or `snippet` on that key
  rather than a `sed -n` on the file. Those three gaps were, measured over a
  long session with the graph attached, most of the reasons an agent reached
  past the verb for a shell command.
- **`snippet` reads a range of a file, and every answer says what to call
  next.** `snippet src/lib.rs:120-180` reads those lines — the `sed -n` that
  a `grep` hit or a body longer than one answer used to send an agent to the
  shell for — and says which symbol the range opens in. A symbol answer now
  ends with the `context` call that expands it, an answer that stops short
  of a body says the exact range that reads on, an ambiguous name comes
  back with the candidates rather than a count, and a miss points at `grep`
  and at the range form instead of leaving the agent to guess. The per-call
  cap rose from 200 lines to 400.
- **The server says what it stands in for.** Its MCP instructions — the text
  every host puts in the agent's system prompt — now open with the rule:
  while the server is connected, `context`, `snippet`, `grep` and `search`
  replace `rg`, `grep`, `cat`, `sed -n` and reading files for code, and a
  shell search is for what no plane holds. The `grep`, `snippet` and
  `context` descriptions say the same in the words an agent is already
  thinking when it reaches for the shell, and `list_planes` reports
  `source_tree` per plane — attached, readable, or absent here — so an agent
  learns whether the verbs can read the source before it tries a shell.
- **`drsg init` installs Claude Code hooks that redirect shell searches to
  the graph.** In the repository's `.claude/settings.local.json` (the
  per-user file), with the scripts beside the plugin store: a `SessionStart`
  hook that puts the rule — drsg first for every code question, shell last —
  in the agent's context where it survives a resume and a compaction, and a
  `PreToolUse` hook on `Bash` that meets an `rg`, `grep`, `cat` or `sed -n`
  on code with the verb that answers instead. `DRSG_RAW=1 <command>` runs
  any command untouched. A config tells an agent where the graph is; these
  tell it when to use it — measured over a long session, the config alone
  did not. Written only when Claude Code shows itself (`.claude/` in the
  repository, or `~/.claude`), idempotent, and other people's hooks in the
  file are left as they are.

### Changed

- **`drsg serve` keeps 20 commits of history, not all of it.** Time-travel
  (`AS OF`, the dashboard's slider) reaches 20 commits back by default;
  versions older than that are reclaimed at compaction. A watched repository
  rewrites a node — embedding and all — every time its file changes, and with
  unbounded history every rewrite stayed on disk and was re-read by every
  compaction: this repository's own database held 402 commits of history in
  175 MiB for a graph whose current state is a fraction of that. `[server]
  retain_commits` sets the window; `0` keeps everything, as before. The
  library default (`Database::set_retention`) is unchanged.
- **`serve watch` keeps its plugins loaded across commits.** The watcher
  loaded the plugin store afresh on every commit, so that a `drsg plugin
  install` in between was picked up without a restart. It still is: the
  loaded set is kept, and reloaded only when the store's registry changed
  since the last load — which every install and remove does.
- **Plugins are compiled once, at install.** `drsg plugin install` now keeps
  the compiled form beside the wasm in the store, pinned by its own SHA-256
  in `registry.toml`, and a load deserializes it in milliseconds instead of
  compiling — which was seconds of CPU and hundreds of MiB for the official
  parsers, paid on every `digest` and on every commit `serve watch` folded.
  A store from an earlier release, or a `drsg update` that changed the
  runtime, compiles on the first load and stores the result for the next.
  With `[plugins] fuel = 0` every load still compiles: the artifact is
  metered, and an unmetered engine cannot run it.
- **Plugins compile on four threads, not every core.** Loading the installed
  parser plugins compiled them on rayon's global pool — one worker per core,
  each holding ~30 MiB of compiler state — and those workers, being global,
  never exited, so their freed memory stayed resident in `drsg serve` for the
  life of the process. Measured here: a digest of a 42-node crate peaked at
  1.04 GiB on 32 cores, 285 MiB on four. Compilation now runs on a pool of at
  most four threads that ends with the load.
- **Compaction moves surviving versions instead of cloning them.** The native
  engine's compaction merged every run into one map and then copied each
  surviving key and value into a second — two copies of the whole store at the
  peak, measured at ~400 MiB over a 175 MiB database. The garbage-collecting
  pass now takes ownership of what it keeps, so the peak is near one copy.

## [2.4.2] - 2026-09-04

### Changed

- **`drsg update` updates the `drsg-mcp` beside it.** With no `--bin` given,
  the command now looks in the directory it is about to install into and, when
  a `drsg-mcp` is there, hands the installer `--bin all` rather than `--bin
  drsg` — saying so on the line before the handover. The two binaries are one
  release, and an install with both was previously left half-upgraded: an
  agent host launching last release's server against this release's `drsg`,
  with nothing to say so, since each binary is individually fine. `--bin`
  still means exactly what it names.

## [2.4.1] - 2026-09-04

### Changed

- **`cypher` on MCP is positioned as the escape hatch, and priced like one.**
  Its description now opens with the pecking order — `context`, `trace`,
  `impact`, `fathom` first on a code plane; `cypher` for aggregations, history
  over a `<name>_git` plane, `AS OF` snapshots, and shapes no verb anticipated —
  and carries the intent plus two examples instead of the whole grammar, which
  every `tools/list` was re-sending to every agent on every turn. The grammar
  moved to where it is needed: a statement that fails to parse comes back with
  the forms of the clauses it reached for (a broken `SEARCH` gets the `SEARCH`
  forms and the `RETURN` tail; a broken `CREATE` gets the writes), so a retry
  is informed rather than guessed.

- **Writes through `cypher` are refused on a mirrored plane.** A plane that
  `digest` or `serve watch` keeps in step with a source tree records the commit
  it reflects; `CREATE`/`MERGE`/`SET`/`REMOVE`/`DELETE` there are refused with
  the reason — the next fold reconciles the plane against the tree, overwriting
  edits to parser-owned nodes and re-creating deletions, so the write would be
  undone without a trace. `write_nodes` / `write_edges` still work on such a
  plane (a fold leaves nodes it does not own alone), and a plane of the agent's
  own takes every statement. The MCP chapter's tool table had listed `cypher`
  as read-only while its description advertised writes; it now says read /
  write, and the safety section says where writes are refused.

- **`RETURN n` on a mirrored plane answers in compact text.** The same
  one-line-per-node rendering every agent verb speaks — a count, the synced
  commit, then key, label, site and score — rather than a JSON record apiece;
  `context` on any key expands it. Projections stay tables everywhere, and a
  plane the agent built for itself keeps returning records.

## [2.4.0] - 2026-09-03

### Added

- **A query can answer with a table.** `RETURN a.name, count(*) AS papers
  ORDER BY papers DESC LIMIT 10` now compiles, runs, and comes back as columns
  and rows — from the MCP verbs, the RPC, `POST /cypher`, the CLI, and as a
  table panel in the dashboard. `RETURN n` and `RETURN *` are untouched: they
  hand back whole records exactly as before, and a plan that doesn't project
  serializes byte-for-byte as the plan it always was.

  Each `RETURN` item becomes a column, named by its `AS` alias or by the item
  as the query wrote it. A column may read any variable the pattern bound, not
  only the last — `RETURN a.name, p.year` across a hop — because a row is a
  current node *plus the trail that reached it*, and the new `Expr::At` term
  names a binding out of that trail. The roadmap had deferred all of this as
  "the most invasive change … it needs the multi-binding row model": the linear
  pipeline turned out to have been carrying every binding all along, and what
  was missing was only a term that names one.

- **Aggregates:** `count`, `sum`, `avg`, `min`, `max` and `collect`, each
  optionally `DISTINCT`, grouped by every column that is *not* an aggregate —
  Cypher's implicit `GROUP BY`, so what a query lists beside its aggregates is
  what it is broken down by. `count(*)` counts rows; the rest read a value.

  Every fold is total, like the rest of evaluation: nulls are skipped, and so
  is a value the fold cannot use — a year one node stored as text doesn't fail
  the query, it just isn't in the average, which is what an aggregate over
  soft-schema data has to do to be worth having. Over a group that gave it
  nothing, `count` and `sum` are `0`, `avg`/`min`/`max` are `null` (the mean of
  no values is unanswerable, not zero) and `collect` is the empty list. A
  projection with no grouping key is one group over the whole match, so
  `RETURN count(*)` answers `0` rather than nothing when nothing matched.

  `sum` stays exact while every value is an integer and is demoted by the first
  float, so summing ids past 2⁵³ doesn't quietly round.

- **`fathom` — an eighth agent verb: read one region of the graph closely.**
  Everything within a few hops of a symbol, out and in, reported as the
  region's *makeup* rather than a listing of it: node counts by label, edge
  counts by type with each direction, how many nodes each hop added, and the
  handful of symbols that hold the region together, ranked by their edges
  *inside* it (a global degree would rank the plane's hubs, not this corner's)
  and excluding the seed, which is the centre by construction.

  The region is the induced subgraph: every edge between two of its nodes,
  including edges among the nodes the last hop reached. Without that closing
  pass a region's last layer looks edgeless — `fathom Ok --depth 1` over this
  repository reported 211 edges and no hub above one edge, where the same
  region actually holds 410 edges and a clear top five.

  It answers a question the other verbs don't: `context` describes one symbol
  and `impact` names what reaches it, while this says what kind of place the
  symbol sits in — a call-heavy corner of the parser, or a shallow leaf. On a
  real Go repository, two hops around `main` comes back as five lines: 519
  nodes, `CONTAINS 360 · CALLS 116 · IMPORTS 71`, and the five hubs.

  Bounded twice, and it says which bound it hit — the depth asked for, or a
  5,000-node budget past which a hub would otherwise pull in the plane. The
  walk is breadth-first, so a truncated region is the *nearest* one, and every
  count is exact over what was walked. `drsg fathom <name> [--depth]` and the
  `fathom` MCP tool.

- **A Query view in the dashboard.** A top-level view beside Dashboard,
  Explore and AIgest: a multi-line editor (⌘/Ctrl+Enter runs, Tab accepts the
  keyword completion) and the result rendered as what it is — a table for a
  projection, with row/column counts, timing and a copy-as-TSV control; a
  record listing for a query that returns nodes; change counts for a write.

  The query surface used to be a tab inside the graph view, labelled "GraphQL
  / Run" — for a language this project does not implement, one unlabelled
  click from a view that opens on Filters. That tab is gone: a query is
  written in one place now, and Explore is the canvas — its remaining tabs
  seed, overlay and search the graph rather than compose queries about it.

- **`type(r)`, `direction(r)` and `id(n)`.** A row's edge and its internal id,
  readable in `WHERE`, `ORDER BY` and a projection. `direction(r)` is never
  `BOTH`: `BOTH` is what the *query* asked for, while a row always walked one
  concrete way. `id(n)` makes a node a grouping key even when it carries no
  projectable property and no external key, which most digested nodes don't.

### Changed

- **`DISTINCT`, `ORDER BY`, `SKIP` and `LIMIT` mean tuples in a projecting
  query.** `RETURN DISTINCT n.file` compares whole projected rows (two nodes in
  one file are one row), `ORDER BY` names a returned column — by alias, or by
  the expression it returned — and the slice counts what the projection
  produced, which is the only reading that can follow an aggregate. Over
  `RETURN n` all four keep their node meaning, unchanged.

- **A `RETURN` this language lacked no longer explains that it lacks it.** The
  errors that used to name projections, aliases and aggregates as unsupported
  shapes described most of what now works, so they are gone. What is still
  unsupported says so precisely: a node cannot share a `RETURN` with columns
  (it is not a value — return its properties), and returning the *rows* of an
  earlier variable still can't be done, though its values project.

- **The docs say which server a repository should use.** `drsg init` — which
  digests the repository, runs `drsg serve … watch`, and writes that URL to
  `.mcp.json` — is the default path, because the plane then follows the
  repository's commits and every agent host shares one instance. `drsg-mcp` is
  the fallback for a host that speaks only stdio or a database nothing
  watches. Pointing both at one database never worked (a database may be
  opened directly by one process at a time) and now says so in the chapter,
  the sample config, and the binary's own `--help` — which now also describe
  the relay above.

### Fixed

- **A digest no longer dies on a provider's concurrency limit.** An account
  that allows five simultaneous chat calls, asked for eight, answers `HTTP 429
  — your current concurrency exceeds your concurrency limit`, and the run
  ended there. The retry it already had was the wrong shape of answer: a 429 is
  two complaints wearing one status code, and backing off answers *too fast*,
  not *too many at once* — every worker that waited came back through the same
  crowded doorway, in step with the others it was refused with, and the run
  died with attempts to spare.

  The provider now keeps a ceiling on what it holds in flight, learned from the
  refusals themselves: the crowd is halved when the provider objects (once per
  crowd, not once per member of it) and climbs back slowly while it doesn't,
  never past the size that was refused. A run settles at whatever the account
  actually allows, without an operator having to know the number. Waits carry
  jitter for the same reason. If a run is still refused at every attempt, the
  error now names the `concurrency` knob and the limit it got down to instead
  of only quoting the provider.

- **`drsg-mcp` parses its arguments.** It had none: the first positional *was*
  the database path, so `drsg-mcp --version` opened a database named
  `--version` — and, since a missing path was created, left an empty one
  behind. It now takes `--db <path>` (a bare path still works), prints
  `--help` and `--version` and exits, and rejects any other dash-led word with
  the usage instead of treating it as a filename.

- **An absent database is an error, not a new one.** The server no longer
  creates the path it was pointed at. An empty graph answers every question
  with "nothing found", which is indistinguishable from a digest that went
  wrong; the message now names the path and the two commands that build one
  (`drsg digest … --apply`, `drsg init`).

- **`drsg-mcp` joins a running server instead of contending with it.** With no
  database named, it reads the nearest `.mcp.json` — walking up, as git finds
  its own directory — and if that file declares a drsg server which answers a
  `GET /health`, it relays the host's stdio session there rather than opening
  a database. In a repository prepared by `drsg init`, a stdio-only host now
  reaches the `drsg serve … watch` that already holds the graph, with a plane
  synced to the repository's commits — where before it lost the race for the
  file and exited, which reaches the host as a bare "connection closed".

  A relay, not a second server: JSON-RPC messages are forwarded as they are,
  in both directions, so the host sees that server's tool set, including tools
  a newer server has and the relaying binary has never heard of. Naming a
  database (`--db`, `$DRSG_DB`) skips the search — a caller who says which
  graph they want is not asking to be sent to another one — and no
  `.mcp.json`, no drsg entry in it, or nothing listening all fall back to
  opening the database here.

## [2.3.0] - 2026-09-01

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

### Fixed
- **`--no-default-features` builds the CLI again.** It had not compiled for
  several releases: the `digest` feature gates `DigestArgs`, `commands::digest`
  and `config::plugin_config`, but not the `Command::Digest` match arm that
  calls them, nor two plugin-store helpers, nor a `PropDesc`/`PropValue` import
  only gated code used. Gated now — and `[plugins]` in `drsg.toml` still
  *parses* in a build with no plugin host, because one config file is shared by
  every binary an operator runs and rejecting a section this build merely has
  no use for would make that file un-shareable.

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
  miscompile", but `RETURN f.file, f.line, key(f)` stopped the parse at the
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
