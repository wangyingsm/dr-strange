# dr-strange — Codebase Analysis

Date: 2026-09-14 · Version analyzed: 2.7.0 (commit `7e13db3`) · 333 tracked files, ~127k lines, 524 commits since 2026-07-22.

Method: every source file was read in full, split across seven parallel review passes (storage, compute/API, parser, LLM, web, CLI/MCP/log, SDKs/benchmarks/docs). The four highest-severity findings were re-verified by hand in the source. **No Rust toolchain is installed on the analysis machine, so nothing was compiled or executed; every finding is traced by reading.** Only the Python SDK drift test could be run (it passes).

---

## Part 1 — Executive summary

### What the codebase is

An AI-native embedded graph database (`drsg`) written in Rust: labelled property graph with first-class vectors, a soft schema, hard plane partitioning, an openCypher-subset language, an MCP server as a primary interface, and an LLM "digest" pipeline that turns documents and source trees into graphs.

| Area | Lines | Role |
|---|---|---|
| `crates/dr-strange-core` | 27,156 | Storage engine trait + 3 backends (native LSM, redb, memory), key encoding, MVCC, HNSW + BM25 sidecars, logical plan, executor, hybrid fusion, cache, public API |
| `crates/dr-strange-web` | 21,436 | axum JSON-RPC 2.0 server (36 methods), `/mcp`, crawler with SSRF guard, replication, Svelte 5 dashboard |
| `crates/dr-strange-llm` | 15,439 | Digest, reconcile/identity/refine, NL→plan "ask", embeddings, wasmtime plugin sandbox |
| `crates/dr-strange-parser` | 7,479 | nom-based Cypher subset + SEARCH/HYBRID/BEAM/CALL, compile to plan, completion |
| `crates/dr-strange-cli` | 7,437 | `drsg` binary, config, self-update, Claude Code hooks |
| `crates/dr-strange-mcp` | 4,395 | 23 MCP tools, stdio + relay transports |
| `crates/dr-strange-log` | 131 | tracing subscriber |
| `sdk/` (6 languages) | ~9,300 | Generated from `openrpc.json` with drift tests |
| `benchmarks/` | 1,297 | Cross-engine harness (SQLite, Kùzu, Neo4j) |

### Overall verdict

Quality is unusually high for a project of this age. Rationale comments record measured numbers and past incidents; corrupt input returns typed `Error::Corrupt` instead of panicking; tests include backend conformance suites run against all three engines, fault-injection durability tests, proptests against a naive model, differential cached-vs-uncached queries, and hostile wasm fixtures. Only 3 TODO markers and 17 `unsafe` sites exist, all with SAFETY comments (SIMD kernels, one `'static` transmute scoped to a single wasm call, `set_var` before threads spawn, platform memory-info FFI).

The main risks cluster in three places: (1) the web layer's security model assumes a loopback bind that the code never enforces, (2) a handful of confirmed correctness bugs in cache/HNSW/parser slot placement, and (3) storage components that are fine at megabyte scale but not gigabyte scale.

### Top findings (ranked)

Items 1–4 and 6–7 were re-verified by hand.

**Security**

1. **Bearer token disclosed to any unauthenticated GET.** `crates/dr-strange-web/src/assets.rs:56` injects `window.__DRSG_TOKEN__` into every HTML response; the SPA fallback (`server.rs:388`) has no auth. On a non-loopback bind, `curl http://host:7700/` yields full Write/Admin. `run()` never checks `addr.ip().is_loopback()`; the Origin-based zero-config fallback (`auth.rs:96`) is forgeable by any non-browser client.
2. **Arbitrary file read via MCP `snippet`.** `crates/dr-strange-mcp/src/lib.rs:1813` does `root.join(file)` with no containment check; `parse_range` (line 1790) accepts absolute paths and `../`. Over `/mcp` on `drsg serve` this is an authenticated remote file read. The symbol branch (lines 1927–1950) reads `file` from node properties anyone can set via `write_nodes`.
3. **SSRF via provider base URL.** `dr-strange-llm/src/openai.rs:237` accepts a raw URL as the provider name and posts via plain `ureq` (line 343), bypassing `fetch/guard.rs`'s `PublicOnly` resolver. Reachable from Read-tier RPC (`plane.ask`, `plane.hybrid`, `plane.find`, `/cypher?embed=`).
4. **Self-update trusts unpinned branch HEAD.** `cli/src/update.rs:490` fetches the installer from `master`; checksum verification is skipped silently when the sidecar is missing (`install.sh:117-131`) and comes from the same origin. `drsg init` writes the literal token into four agent config files, only one of which is gitignored (`commands.rs:690-766`).
5. **Prompt injection → graph ownership.** `llm/src/digest.rs:883-892` merges any property name the model emits, including `_generated_by`; a poisoned document can make a node parser-owned so `sync.rs:130` deletes it on the next fold. Plugin `Host::read` (`preprocess/mod.rs:417`) bypasses the `.gitignore`/hidden filter that `list` honours, so a plugin can read `.env` inside the tree.

**Correctness**

6. **Cross-plane cache leak.** L2 keys are bare ids (`core/src/api/cache/store.rs:26`); `CachedReader::node/edge/neighbors` return an L2 hit without checking `plane` (`cache/mod.rs:340, 358, 377`). Ids are global, so a foreign id must be known, but `planeB.query().seek_ids([id_in_A])` returns A's record where the uncached path returns `None`.
7. **HNSW panic after deleting the entry point.** `core/src/storage/hnsw.rs:810` replaces `entry` with an arbitrary live node but never lowers `top_layer`. The index's own `is_wellformed` (line 241) names this state as one where "searching such a graph panics". ~94% of nodes have only layer 0, so the first delete of the entry node is likely to trigger it.
8. **Commit reports failure after it is durable.** `native/mod.rs:1004-1007` returns flush/compact errors after publishing; `api/mod.rs:1879` then `?`s out before applying index/keyword events, so HNSW/BM25 silently diverge until reopen.
9. **Time-travel keeps the live BM25 index** (`api/mod.rs:948-953`): `AS OF` keyword/hybrid queries see latest postings.
10. **Parser slot bug.** Variable-free predicates (`score()`, `hops()`) go to slot 0, before any Expand (`parser/src/compile.rs:93`), so they filter on Null and return nothing. Also: `ORDER BY count(*)` is a syntax error despite docs; MERGE keys can't be parameterised and strings have no escapes (injection via `key(n) = "..."`); the completer slices mid-codepoint on non-ASCII (suspected panic, `complete.rs:257`).
11. `FrontierTopK` can seat the same node in several of the k slots (`exec.rs:578-584`); hybrid graph channel ignores `label` (`hybrid.rs:283`); in-memory `restore` never loads sidecars (`snapshot.rs:342`).

**Durability and scale**

12. Default retention is unbounded (`native/mod.rs:431`); only `drsg serve` calls `set_retention`, so CLI-only databases never GC versions and every commit rewrites the counters row.
13. Compaction merges every run in memory (`native/mod.rs:511`); range reads materialise the whole range (line 799); flush fsyncs under the store write lock (line 386); no directory fsync after SST rename; SST data blocks carry no checksum, so bit-rot reads as "absent" (`sst.rs:431`).
14. `drop_plane` leaks `vidx:`/`kidx:` declarations (`api/mod.rs:805`); every `record_query` bumps the commit seq and flushes the whole L2 cache (`api/mod.rs:605`).

**SDKs and benchmarks**

15. No SDK is published despite `sdk/README.md` install commands; versions disagree (pom `2.0.0-alpha`, Java README `0.1.0`, pyproject `2.0.0a0`, openrpc `0.1.0`, workspace `2.7.0`). C SDK `malloc`s a server-controlled 64-bit frame length (`drsg.c:304`); Go `Watch` leaks a goroutine when the server closes first (`watch.go:84`); Java `Client` has no `close()`.
16. Benchmarks time drsg in-process against Python-driven engines; Neo4j `vector_build` includes loading while drsg's excludes it; no recall measurement; results JSON is gitignored, so `BENCHMARKS.md` isn't reproducible.

### Suggested order of work

1. Refuse non-loopback binds without a token; never inject the token for non-loopback binds; gate the zero-config fallback on `addr.is_loopback()`.
2. Add root containment (canonicalize + `starts_with`) to `read_range` and the symbol branch of `snippet`; refuse absolute paths.
3. Reset `top_layer` in `HnswIndex::remove`; include the plane in the L2 cache key (or compare `record.plane`).
4. Route provider URLs through `guard::precheck` + `PublicOnly`, or restrict RPC to named presets.
5. Default variable-free predicates to the last slot in the compiler; add string escapes and `{key: $k}` MERGE support.
6. Set a default retention in the CLI open path; add per-entry caps to the L2 cache.
7. Apply index events before reporting commit failure, or make the post-publish maintenance errors non-fatal to the caller.

### Roadmap status

`ROADMAP.md` marks §1–§10 and §13 shipped. §11 (preprocessor plugins) is mostly shipped with two open items (tags/branches without a commit reach the history plane late; no code↔history plane link). §12 (scoped identity: tokens, `TokenStore`, `Principal`, `--mcp-addr`, audit) is entirely open with five unsettled forks. Deferred: `WITH`-pipelining and schema constraints.

---

## Part 2 — Detailed area reports

### 2.1 Core storage layer

Paths under `crates/dr-strange-core/src/`.

#### Architecture

**Trait seam.** `storage/engine.rs:118-190` defines `StorageEngine` (GAT read/write txns, `set_write_timeout`) and dyn-compatible `ReadTransaction`/`WriteTransaction` (`get`, half-open `range`, `put`, `delete`, `put_batch`, `delete_prefix`, by-value `commit`). Eleven logical `TableId`s (`engine.rs:45-57`); the graph layer (`storage/graph/`) is written once against `&dyn` txns.

**Backends.** `memory.rs` (Arc-COW snapshots, `Mutex<()>` single writer), `redb_backend.rs` (legacy, feature-gated), and the default `native/` LSM.

**Key encoding** (`keys.rs`): big-endian fixed-width `plane·id` prefixes so per-plane scans/drops are contiguous ranges; adjacency keys `plane·src·type·dst·edge` (`keys.rs:462-476`) make 1-hop expansion a prefix scan and allow parallel edges. Meta table holds counters, dictionaries, `vidx:`/`kidx:` index declarations, query history, `commit_seq`/`commit_time`. Records are postcard (`codec.rs`) with a `format_pin` test.

**Planes** are a hard partition: `insert_edge` rejects endpoints not in the plane (`graph/edge.rs:422-429`); `drop_plane` is a `delete_prefix` per table plus a `node_plane` sweep (`graph/meta.rs:1349-1389`).

**Native LSM** (`native/mod.rs`). Memtable is `BTreeMap<(table, key, Reverse(seq)), Op>`; a write txn stages into `buf`, and `commit` = one length+CRC32-prefixed postcard WAL batch, `fsync`, observer notify, then publish under the store write lock (`durable_commit`, :946-1008). MVCC by sequence: readers pin `committed_seq` and register in `readers` (:693-714) so compaction can compute a floor. Flush writes an SST (data blocks, index, Bloom, CRC'd footer; `native/sst.rs`) via temp+fsync+rename, then truncates the WAL (:386-408). Compaction merges all runs in memory once >4 exist and GCs versions below `min(oldest reader, retention floor)` (:561-621). Single-writer is a `Condvar` gate with optional deadline (:57-127); cross-process exclusivity is an `fs4` lock on `<dir>/LOCK` (:210-242). Time travel: `begin_read_at` (:448-474). Replication: `apply_replicated` lands the master's seq verbatim (:1039-1058).

**Sidecars.** `index.rs` (HNSW registry) and `keyword.rs` (BM25) are in-memory, rebuilt from the KV on open unless a `.hnsw`/`.bm25` sidecar's stamped seq equals the KV's `commit_seq` (`api/mod.rs:510-560`). HNSW (`hnsw.rs`) is hand-rolled with SIMD dot kernels (`vector.rs:94-349`), tombstone deletes, and a lock-per-node parallel builder.

**Migration ladder.** `graph/meta.rs:947-1007`: refuse newer, refuse < `MIN_SUPPORTED_VERSION`, step `migrate_step` per version. Currently v2==min, so the ladder is empty.

Note: `compact.rs` is agent-facing text rendering, not storage compaction.

#### Code quality

Strengths: design rationale and measurements in comments; backend-conformance suite run against all three engines including a 1-byte flush threshold (`conformance_tests.rs:2243-2260`) and a pinned-reader-through-compaction test (:2266-2315); fault-injection durability tests (`tests/durability.rs`); proptest model tests (`tests/model_pbt.rs`) including a memory-vs-disk differential; codec fuzz/roundtrip/format-pin tests; every corrupt-input path returns `Error::Corrupt`. `unsafe` is confined to SIMD kernels, gated on runtime CPU detection, guarded against length mismatch (`hnsw.rs:792-797`).

Gaps: no crash-consistency test that kills mid-flush/compaction (acknowledged in arch); no test that removes the HNSW entry node then searches; no direct WAL torn-tail replay test; `gc_versions` tests don't cover retention=0.

#### Concerns

Confirmed:

1. **HNSW panic after deleting the entry point** — `hnsw.rs:803-814` replaces `entry` but leaves `top_layer`; the next `search`/`insert` starts at `top_layer` on a node that may have fewer layers → index OOB. Reached via `remove_node` on node delete.
2. **Commit reports failure after it is durable and visible** — `durable_commit` returns `maybe_flush`/`maybe_compact` errors (:1004-1007) after publishing; `api/mod.rs:1838+` then `?`s out before applying index/keyword events.
3. **Default retention never reclaims anything** — `retain_commits=0` ⇒ `retention_floor` returns 0 (:429-434) ⇒ every version kept. Only `drsg serve` calls `set_retention` (`web/src/server.rs:1085`).
4. **Flush I/O under the store write lock** — `maybe_flush` (:386-408) writes+fsyncs an SST while holding the `RwLock` writer.
5. **Compaction is fully in-memory** — `maybe_compact` loads every run into one `BTreeMap` (:511-515).
6. **Range reads are fully materialized** — `committed_values` (:799-815) builds a `BTreeMap` of the entire range across all SSTs.
7. **Silent truncation on SST data-block corruption** — blocks have no checksum; `decode_entry` returning `None` ends the loop quietly (`sst.rs:431,482,516`).
8. **`drop_plane` leaks index declarations** — neither `graph::drop_plane` nor `api::drop_plane` (`api/mod.rs:805-810`) removes `vidx:`/`kidx:` rows.
9. **Bulk-load external-key uniqueness not checked against the KV** — `graph/bulk.rs:630-633, 747-757`; a duplicate overwrites the `ext_keys` row while the old node keeps its inline key; deleting the old node later removes the new node's entry (`node.rs:340-342`).
10. **WAL record length cast** — `body.len() as u32` (:1110) wraps for a >4 GiB batch.
11. **Per-query scratch allocation** — `search_with_ef` zeroes a `Vec<u32>` of N per query (`hnsw.rs:1011`).
12. **`Sst::read_block` serializes on `Mutex<File>`** (`sst.rs:444-448`).

Suspected:

13. **No directory fsync** after SST rename (`sst.rs:271`) or WAL truncate (:405).
14. **Replica sequence regression** — `durable_commit` sets `committed_seq = seq` unconditionally (:1003); `Database::init` performs two local bootstrap commits on every open (`api/mod.rs:516-531`), so a replica's seq can exceed the master's and be moved backwards by `apply_replicated`.
15. **Blocking observer under lock** — `wal_observer` is invoked while its mutex is held (:975-993).

#### Design decisions to flag

- Single-tier "merge everything" compaction trades write amplification and memory for simplicity.
- Two commit sequences (engine `seq` vs KV `META_COMMIT_SEQ`) are deliberately separate; easy to confuse.
- Every open writes (two bootstrap commits), including read-only replicas.
- Sidecar as pure cache with seq-equality gating means a full HNSW rebuild after any write followed by unclean shutdown.
- HNSW tombstones never reclaimed in-process.
- Lock poisoning ignored everywhere; a panic mid-`durable_commit` between WAL append and publish leaves a durable record invisible until reopen.
- Lock-filesystem fallback allows opens when locking is unsupported (:232-240).

### 2.2 Core compute, cache and API

Paths under `crates/dr-strange-core/`.

#### Architecture

**Logical plan** (`src/compute/plan.rs`): `LogicalPlan = Source + Vec<Step> + Option<Projection>`. Sources: scans/seeks, `VectorTopK`, `KeywordTopK`, `Hybrid`, `Algo` (plan.rs:30-66). Steps are a linear pipeline over a "current node + trail + optional f32 score" row (plan.rs:109-155). `Projection` is a tail carrying aggregates with implicit GROUP BY (plan.rs:286-295). Everything is serde-serializable and `#[non_exhaustive]`; `binding_need()` (plan.rs:328) precomputes which `Expr::At` slots the plan names.

**Expression evaluator** (`src/compute/expr.rs`): total evaluation — never errors; missing/mismatched yields `Null`/`false` (expr.rs:544-646). Two orderings: `partial_cmp` for filters (SQL-ish, NaN/mixed ⇒ false) and a genuine `total_cmp` for sort/grouping with exact i64-vs-f64 comparison (expr.rs:698-801).

**Executor** (`src/compute/exec.rs`): pull-based `Box<dyn Iterator<Item=Result<Row>>>` chain; source ids materialized, steps lazy except barriers (`Sort`, `FrontierTopK`, `ExpandBeam`). Trail is an `Rc` cons-list so `step()` is O(1) (exec.rs:42-108). Deadline is a row-paced wrapper checked every 1024 rows (exec.rs:246-326). `execute_table` handles projection/aggregation via `TupleKey` ordered by `total_cmp` (exec.rs:783-1126).

**Hybrid** (`src/compute/hybrid.rs`): min-max-normalized weighted fusion of vector/BM25/graph-proximity channels (hybrid.rs:109-154).

**Cache** (`src/api/cache/`): `GraphReader` trait seam (mod.rs:49-94); `UncachedReader` oracle; `CachedReader` = per-query L1 `RefCell<AHashMap>` (incl. negative caching) over a shared moka L2 `GraphCache` whose entries are stamped with the commit seq and served only on exact seq match (store.rs:5-16, mod.rs:336-389).

**API** (`src/api/mod.rs`): `Database` (engine enum + `RwLock<VectorRegistry>` + `RwLock<KeywordRegistry>` + `GraphCache` + change observer) → `PlaneHandle<'db>` (Copy; carries `as_of` and `deadline`) → `QueryBuilder`/`AlgoBuilder`/`HybridBuilder`/`WriteTxn`. Write txns buffer index events, counter deltas and change records, applied after KV commit (mod.rs:1838-1906).

**Catalog** (`src/compute/catalog.rs`): scan-computed `CatalogSnapshot` plus transactionally maintained `PlaneCounters`. **JSON dialect** (`src/json.rs`): `$vector`/`$bytes`/`$desc+$value` escapes and a "lean" shape.

#### Code quality

Strengths: `total_cmp_is_a_total_order` brute-forces antisymmetry/transitivity (expr.rs:1004); the 2⁵³ trap (expr.rs:1055); differential cached-vs-uncached test (cache/mod.rs:573); proptest against a naive model (tests/query.rs:404); dual-backend suites; `PoisonError::into_inner` everywhere; append-only wire-format pins (text.rs:457).

Gaps: no test for cross-plane isolation through the query path; no test for `FrontierTopK` with duplicate heads; no test of hybrid graph channel respecting `label`; `UncachedReader` has no `keyword_search`, so keyword paths have no differential oracle; tests named `*_redb` actually run the native backend (tests/hybrid.rs:147, query.rs:358, smoke.rs:105, cache.rs:74).

#### Concerns

Confirmed:

1. **L2 cache leaks records across planes** (store.rs:25-29; cache/mod.rs:340-345, 358-362, 377-381).
2. **Time-travel keeps the live BM25 index** (api/mod.rs:948-953).
3. **Hybrid graph channel ignores `label`** — `graph_proximity` expands `Dir::Both, None` (hybrid.rs:283), contradicting the `HybridSpec::label` doc (hybrid.rs:67-69).
4. **`FrontierTopK` with duplicate heads** — `by_head` collapses duplicates (exec.rs:578-579) but candidates keep them; the second copy gets a trail-less row (exec.rs:584).
5. **`WriteTxn::commit` returns `Err` after a durable commit** if `apply_index_event` fails (api/mod.rs:1879-1884).
6. **In-memory `restore` never loads sidecars** — gated on `self.sidecar.is_some()` (api/snapshot.rs:342-353).
7. **Every `record_query` flushes the whole L2** — it is a `with_write` that bumps the commit seq (api/mod.rs:605-609).
8. **`ExpandVar` is fully eager per start row** (exec.rs:709-726): all walks materialize before `Limit` or the deadline sees a row.
9. **`catalog::compute` decodes both endpoint nodes per edge** through the raw txn, no cache (catalog.rs:252-257).
10. Minor: JSON `u64 > i64::MAX` wraps negative (json.rs:31-32); a map with a literal `$value` key cannot round-trip (json.rs:56-58); `Skip/Limit` `u64 as usize` (exec.rs:555-556); `frame.out.clone()` for `Dir::Out` (algo.rs:284).

Suspected:

11. **Registry/snapshot skew window.** Reader takes the registry read lock then opens the txn (api/mod.rs:931-947); writer commits KV then takes the write lock (mod.rs:1872-1884) ⇒ phantom ids from `vector_search`.
12. **Restore/replication can reuse a seq the cache already stamped** (snapshot.rs:334).
13. **No per-entry cap on L2** despite arch/02 §4 (store.rs:113-121).
14. `fuse` uses `partial_cmp().unwrap_or(Equal)` (hybrid.rs:240-244); an `inf` distance makes the comparator non-total.
15. Terminals hold both registry read locks for the whole query (mod.rs:931-955); writer-preferring `RwLock` can starve new queries.
16. Change feed on bulk load pushes one tuple per node then collapses all before truncating to 256 (mod.rs:1475-1477, 1937-1949).

#### Design decisions to flag

- Exact-seq stamping makes any write, including metadata writes, a global cache flush.
- Walk semantics for `ExpandVar`/`ExpandBeam` (revisits allowed) plus eagerness is the main runaway-query risk for LLM-authored plans.
- Min-max normalization: the worst candidate of each pool always scores 0; seeds get full graph boost, double-counting top hits (hybrid.rs:266-268).
- Total expression semantics make `NOT (p CONTAINS x)` true for missing `p` and `Null = Null` true.
- Score channel is a single `f32`; PageRank ranks and `Algo` indices are cast into it.
- `bulk_load` trusts external-key uniqueness (api/mod.rs:1454-1456) and `bulk_load_edges` trusts endpoint existence (1528-1529).

### 2.3 Cypher parser

Paths under `crates/dr-strange-parser/`.

#### Architecture

Hand-composed `nom` 7 grammar (`src/parse.rs`) over `&str`, no separate lexer; every token helper leads with `multispace0` (parse.rs:23-49). Expression precedence climbing `or < and < not < comparison < additive < multiplicative < unary < primary` (parse.rs:127-300). Top level `statement = alt(create | merge | match-write | read)` (parse.rs:1320); reads `alt(match | search | hybrid | call)` sharing one `query_tail` (parse.rs:639).

AST (`src/ast.rs`) keeps variable qualifiers so the compiler can place each predicate. Compile (`src/compile.rs`): WHERE split on top-level AND (compile.rs:715), each conjunct pushed to the slot of its single variable (compile.rs:88-108); earlier variables in projections become `Expr::At` (780-805); `key(n) = "…"`/`IN [...]` on a scanned source rewrites to `Source::SeekKeys` (110-135); `RETURN n|*` keeps node rows, anything else becomes a `Projection` tail (196-300).

`complete.rs` is a second permissive reading (byte tokenizer + `Pos` state machine ranked by a `Vocab` catalog); `hint.rs` picks grammar snippets by keywords. Writes (`src/write.rs`) are imperative: MATCH compiles to a read plan, ids are fetched, ops apply in one `WriteTxn`. MERGE upserts by external key with a statement-scoped cache (write.rs:250-254).

Supported: single linear path, bounded var-length, WHERE ops in lib.rs:41-45, string predicates, `IN`, `IS [NOT] NULL`, 6 aggregates, ORDER/SKIP/LIMIT, `AS OF`, plus SEARCH/HYBRID/CALL/BEAM. Explicitly unsupported (lib.rs:81-86): cross-variable predicates, non-terminal rows, WITH, unbounded `*`. Undocumented absences: OPTIONAL MATCH, UNION, multiple MATCH, branching patterns, relationship variables (discarded, parse.rs:490), list/map literals, `%`/`^`, backticked identifiers, string escapes.

#### Code quality

Strengths: every unsupported shape is a clear `Compile` error with a suggested rewrite. Tests build expected plans with core's own builders (`tests/parse.rs`, 99 tests). `tests/write.rs` (31) is end-to-end against an in-memory DB including rollback. `tests/complete.rs` (35) cuts a query at every byte, accepts the best suggestion repeatedly, and requires the result to parse (complete.rs:609).

Gaps: no property/fuzz tests, no golden files; error-message tests are substring checks (tests/parse.rs:1545-1576); RFC-3339 hand parser (parse.rs:938-1010) has three positive cases.

#### Concerns

Confirmed:

1. **`ORDER BY count(*)` is a syntax error**, contradicting ast.rs:218-220 and lib.rs:52-53 (`order_key` at parse.rs:612; `expr` rejects `count(` at parse.rs:333). Only aliases work.
2. **`score()`/`hops()` in WHERE evaluated at slot 0** (compile.rs:93), before any Expand — silently returns nothing.
3. **Parameterised MERGE key impossible.** Only a literal string `key:` becomes the external key (parse.rs:1117); `{key: $k}` is rejected by `validate_merge` (write.rs:155). No string escapes (parse.rs:106,411) ⇒ injection via `key(n) = "` + `x" OR 1 = 1` + `"`.
4. **`shortest_path(weight: "Cost")` lowercases the property name** (compile.rs:583, 642).
5. **Function names case-sensitive while keywords aren't** (parse.rs:333).
6. **Error attribution for writes blames token 0** (lib.rs:256-268, parse.rs:1320).
7. **NULL semantics are two-valued**: `<>`, `NOT`, and `IN`→`OR` expansion (compile.rs:825-843) include rows lacking the property, unlike openCypher.
8. Identifiers ASCII-only (parse.rs:43), no backtick escape; no exponent floats (parse.rs:94).

Suspected:

9. **Panic on non-ASCII in `complete`** — `tokenize` casts bytes to `char` (complete.rs:234,253) and slices `&prefix[start..i]` (257) mid-codepoint.
10. **Stack overflow on deep nesting** (parse.rs:295) — no depth limit.
11. **Duplicate-row writes**: `ids()` does not dedupe (api/mod.rs:2343); `DETACH DELETE n` reached twice likely errors; `ensure_edge` checks only the committed store (write.rs:534).
12. `iterations`/`max_levels` truncate `i64 → u32` (compile.rs:554,565).

Backtracking is bounded; no exponential blow-up found.

#### Design decisions to flag

- Filter pushdown by single-variable slot is elegant but brittle (see #2).
- Params resolved at parse time into literals ⇒ plans not cacheable across parameter values.
- Writes bypass the plan — no explain/serialize path for mutations.
- Two grammars (`parse.rs`, `complete.rs`) plus `hint.rs` must be kept in sync by hand.

### 2.4 LLM crate

Paths under `crates/dr-strange-llm/`.

#### Architecture

**Provider** (`src/provider.rs:11-45`): `Chat::complete(system, user)` and `Embedder::embed(&[String])`, plus typed `OutputTruncated`. `OpenAiProvider` (`src/openai.rs`) is a synchronous `ureq` client; `src/preset.rs` maps `openai|deepseek|qwen|ollama`; `build_provider` (openai.rs:218) also accepts a raw URL. Retries (4 attempts, exponential backoff + jitter, `Retry-After`), timeouts (300s chat / 60s embed), adaptive `Throttle` learned from 429s (openai.rs:79-172).

**Document reading** (`src/document.rs`): bytes → Markdown via pinned `anydoc = "=0.1.8"`, format sniffed from bytes.

**Digest** (`src/digest.rs:409`): paragraph-aware chunking → Phase A entity-linking context → Phase B parallel extraction with recursive re-split on truncation (:1097) → Phase C ordered merge → `reconcile.rs` → `identity.rs` → `refine.rs` (Super mode) → exact key check → drop dangling relations → embed → stamp `_source/_model/_run`. `DigestResult::apply` (:343) bulk-loads.

**Ask** (`src/ask.rs:78`): agentic loop (default 20 turns) with `find_edge`/`find_entity` tools; final `LogicalPlan` is deserialized and executed via `query_from_plan`. **Vectorize** (`src/vectorize.rs`): incremental embedding keyed by `_embedded_from` sha256 prefix.

**Preprocess / plugins** (`src/preprocess/`): `route_document`/`route_tree`/`route_paths` (mod.rs:678-801). `Host` trait (mod.rs:227) = `list/read/label`; `LocalFiles` canonicalizes and checks `starts_with(root)` (mod.rs:342-352). `wasm.rs` hosts wasmtime components against `wit/preprocess.wit`: empty preopen table, `wasi:sockets` refused (wasm.rs:196), frozen clocks, fixed entropy, captured stderr, fuel (200 G default), per-store memory (3 GiB). `registry.rs` = per-user store with SHA-256 pins and precompiled `.cwasm`; `catalog.rs` = fetched official list; `ground.rs` = facts-beat-model fold; `ledger.rs` = last-run report; `repo.rs` = `git` plugin; `sync.rs` = whole-tree re-route with diff-apply.

#### Code quality

Strengths: determinism treated as a contract (sorted walks, BTreeMaps, frozen clocks). LLM outputs validated against known sets (reconcile.rs:158, identity.rs:205). Provenance hidden from prompts and protected in `refine::apply` (refine.rs:216). Failures reported, not swallowed.

Tests: `MockProvider` (provider.rs:55) serves canned replies; `tests/digest.rs:13-77` `Scripted` dispatches by system-prompt prefix (brittle). `openai.rs:639+` tests retry/throttle against a real local TCP server. Sandbox proven against committed hostile wasm fixtures (`tests/sandbox.rs`). Gaps: `vectorize.rs` untested; `parse_extraction` malformed-input untested; `extract_all` abort untested.

#### Concerns

Prompt injection → graph writes:

- Confirmed: `merge_props` (digest.rs:883-892) inserts any property name including `_`-prefixed and `embedding`; `add_provenance` (:896) overwrites only `_source/_model/_run`. `refine::apply` filters these; extraction does not.
- Confirmed: `ExEntity.key` unvalidated (digest.rs:160); `""` passes to `BulkNode`.
- Confirmed: graph content flows back into prompts (digest.rs:1258, ask.rs:398, 160) — second-order injection.
- Confirmed: `ask` read-only safety rests on `Step` having no mutation variants, not on `read_only()` as `arch/07-llm.md §3.3` states. `ensure_limit` (ask.rs:427) only appends when no `Limit` exists; a model can emit `Limit(10^12)`.

Sandbox:

- Confirmed: `Host::read` bypasses the ignore policy (mod.rs:366-398 vs :417).
- Confirmed: memory limit is per store (wasm.rs:561); `CHUNK_FILES = 1` (:104) with `par_chunks` on rayon's global pool (:646) ⇒ worst case `cores × 3 GiB`.
- Confirmed: no wall-clock deadline; a `read` on a FIFO blocks forever.
- Suspected: only `memory_size` set on `StoreLimitsBuilder` (:559-562); table growth unbounded.
- Confirmed, acceptable: `transmute` to `&'static dyn Host` (wasm.rs:526) and `unsafe Component::deserialize` (:372).
- Suspected, low: plugin `version` unvalidated in filenames (registry.rs:171).

Secrets: keys come only from env (openai.rs:247), no `Debug` on `OpenAiProvider`. Suspected: transport error strings logged at openai.rs:405 could echo a key embedded in a user-supplied base URL query string.

Retry/timeouts: `Retry-After` clamped to 8s (openai.rs:384-393), contradicting the comment. `extract_all` has no abort flag (digest.rs:1076-1083).

JSON robustness: `parse_extraction` (digest.rs:1271) is first-`{`-to-last-`}`; hard abort on failure, whereas reconcile/identity/refine swallow parse failures silently. Suspected: `chat_body` always sends `temperature: 0` and `max_tokens` (openai.rs:452-461), rejected by some reasoning models.

Concurrency/perf: `containment_pairs` recomputes `fold_key(outer)` in an O(n²) loop (identity.rs:122); `vectorize_plane` holds every text and vector in memory (vectorize.rs:100-131); `sync_paths` commits two transactions (sync.rs:266,336); `record_ledger` read-modify-writes unguarded (ledger.rs:137-145); `LivePlugins` stamps on mtime+len (registry.rs:130).

Stale docs: lib.rs:11 TODO; ask.rs:29 says default 6, code is 20 (:41); sync.rs:104 `_delta` unused; tests/wasm_smoke.rs:5 references "task #17".

#### Design decisions to flag

- Precompiled artifacts require fuel on (wasm.rs:358-368): `fuel = 0` silently recompiles every load.
- Whole-tree re-route per commit in watch mode.
- Edges never deduplicated across facts and model (ground.rs:145-146).
- Descriptions truncated to 160/140 chars in prompts.
- `Scripted` mock keyed on prompt prefixes.

### 2.5 Web crate and frontend

Paths under `crates/dr-strange-web/`.

#### Architecture

**Server.** `lib.rs:227` `serve()` builds a multi-thread tokio runtime and calls `server::run` (server.rs:1022). `router()` (server.rs:319-391): `/rpc`, `/ws` (JSON-RPC over WebSocket + `db.stats` push every 2s + `plane.watch`), `/mcp` (rmcp Streamable HTTP, 268-317), web-only `/cypher`, `/cypher/complete`, `/cypher/history[/{id}]`, `/digest/extract`, `/digest/fetch`, `/export`, replication `/snapshot` + `/ws/wal`, unauthenticated `/health`, `fallback(static_handler)`. Hardening (330-345): `CatchPanicLayer`, `GlobalConcurrencyLimitLayer(1024)`, nosniff/`X-Frame-Options: DENY`/`Referrer-Policy`, `DefaultBodyLimit` 64 MiB (`RequestBodyLimitLayer` separately on `/mcp`). TLS via axum-server/rustls (1253). Every core call goes through `spawn_blocking`.

**Dispatch.** `rpc.rs:92 handle()` is pure/sync; `dispatch_method` (235-303) wraps every arm in `guarded!(Access::…, handler)`. 36 methods; a test (rpc.rs:1183) asserts OpenRPC, `METHODS`, and dispatch agree. `Ctx::plane()` (methods.rs:47) stamps the per-request deadline.

**Auth.** `auth.rs`: `Authorizer` trait, `SharedToken` (constant-time compare, 102), `ReadOnlyAuthorizer` for `--follow`. `Credentials{bearer, local_ui}` resolved in `server.rs:175`: disallowed `Origin` → 403; allowed loopback/`DRSG_ALLOWED_ORIGINS` origin → `local_ui=true`; no Origin → native client. With no token, only `local_ui` passes (auth.rs:96). Token rides `Authorization: Bearer` or `?token=` on WS. Token injected into `index.html` (assets.rs:106). No CORS headers emitted.

**MCP.** One `DrStrange` per session sharing `Arc<Database>`, process-wide tool semaphore (282), idle/init timeouts (237-239), `mcp_auth` middleware at Write tier (206).

**Assets.** `build.rs` writes a placeholder `index.html` if absent so `rust-embed` (assets.rs:15) always compiles.

**Fetch.** `fetch/mod.rs` crawls with ureq, budgets, `robots.rs`, `html.rs` (scraper → Markdown), `relevance.rs` (BM25-ish), `guard.rs` `PublicOnly` resolver filtering resolved IPs on every hop.

**follow.rs.** Subscribes `/ws/wal` first, pulls `/snapshot`, `db.restore`, drains/forwards batches.

**Frontend.** Svelte 5 runes + Vite/bun. `App.svelte` owns plane/search/`asOf`; `Dashboard`, `Explore` (sigma/graphology), `Query`, `Digest`, `CreatePlane`. `rpc.js` 69-line client; `layout.js` leaf folding/sectoring/focus fade (pure, tested); `markdown.js`/`json.js` escape-first renderers.

#### Code quality

Strengths: access level named at every dispatch arm; deadline applied centrally; sync core never on the async executor; body limits on all routes with regression test (`mcp.rs:290`); constant-time token compare; resolver-level SSRF guard; escape-first renderers; lean vector marker (server.rs:789-795).

Tests: `rpc.rs` ~60 unit tests, `auth.rs`, `assets.rs`, all `fetch/*`, `tests/http.rs`, `tests/mcp.rs`. Gaps: no tests for `/ws`, `/ws/wal`, `/snapshot`, `/export`, `/digest/fetch`, TLS, `follow.rs`. Frontend: pure modules tested; no component tests; `rpc.js`/`plot.js` untested.

#### Concerns

Confirmed:

1. **Bearer token disclosed to any unauthenticated GET `/`** (assets.rs:52-58, 77-78; server.rs:388).
2. **Zero-config fallback not gated on loopback bind** (auth.rs:96); arch/08 §4.2 invariant 2 unimplemented.
3. **SSRF via provider base URL, bypassing `guard`** (openai.rs:237, 343; reachable at server.rs:814, methods.rs:955, 1624, 1712, 1844, 2051-2063, 519).
4. **No rate limiting / brute-force protection.** Batch size unbounded (rpc.rs:112).
5. **Cost/DoS amplification via client knobs.** `digest.run` `concurrency`/`chunk_chars` uncapped (methods.rs:2075-2077); `plane.ask` `max_attempts` default 20 vs doc 3 (methods.rs:1718 vs 1684); `/snapshot` and `/export` buffer the whole DB (server.rs:668, methods.rs:2374).
6. **Information leaks in errors** (methods.rs:63-70, 526, 552, 576; server.rs:683, 843, 882).
7. **`plane.find` loads every node record before capping** (methods.rs:1381-1382), fired on every header keystroke; `graph.seed order=degree` looks up neighbours for every node per seed (methods.rs:1252-1256).
8. **WS token in the query string** (server.rs:1354, rpc.js:36).
9. **XSS surfaces are sound** (`markdown.js`, `json.js`, `SAFE_URL`). No CSP header.
10. **Blocking runtime** consistently avoided; `plugin_catalog` spawns a bare `std::thread` per stale hit (methods.rs:441).

Suspected: `guard.rs:149` omits NAT64 `64:ff9b::/96`; `follow.rs:69,105` unbounded mpsc channels; `showAll()` (Explore.svelte:443) runs ForceAtlas2 synchronously on the main thread; `expandOne` issues up to 300 parallel RPCs (Explore.svelte:460); `/mcp` rejects any non-loopback `Host` with no knob to allow LAN (mcp.rs:239-260).

#### Design decisions to flag

- Origin-as-auth is the load-bearing wall; recommend refusing non-loopback start without a token and never injecting the token for non-loopback binds.
- Single shared token, three tiers; `plane.cypher` Write-gated even for reads, `plane.ask`/`hybrid` Read-gated while spending money.
- JSON-RPC errors return HTTP 200 while web-only routes use 401/403/400.
- Web-only endpoints outside OpenRPC mean SDKs can't reach paging/lean toggles.
- Whole-DB in memory for `/snapshot`, `/export`, `plane.find`.
- `retain_commits` default 20 silently caps time-travel depth the UI slider advertises.

### 2.6 CLI, MCP and log crates, hooks, CI

#### Architecture

**CLI** (`crates/dr-strange-cli/src/main.rs:27-739`): `init`, `plane {list,create,drop,show}`, `import/export`, `get`, `query`, `cypher`, `queries`, agent verbs `context/describe/trace/impact/fathom/snippet/grep/traverse/history/search`, `catalog`, `algo`, `hybrid`, `index`, `stats/check`, `snapshot/restore`, `serve [watch] [--follow]`, `ask`, `vectorize`, `update`, `plugin`, `digest`. `snippet/grep/traverse` reuse `dr_strange_mcp::*_logic` (main.rs:928-1016).

**Config precedence** (config.rs:218-235, 244-264): `--config` > `$DRSG_CONFIG` > `./drsg.toml`. File values folded into the process environment via `set_var` when unset; `[llm]` keys become env vars.

**Self-update** (update.rs): resolves latest tag via GitHub redirect through `redirect_target` (private-address guard), compares numerically (609-644), then `exec`s `curl … | sh -s -- --bin X --dir '<own dir>'` (660-720).

**MCP server** (`crates/dr-strange-mcp/src/lib.rs`): `DrStrange` wraps `Arc<Database>`; each `#[tool]` runs on `spawn_blocking` behind a shared `Semaphore` (544-574). 23 tools. Transports: stdio via `drsg-mcp` (`with_local_files(true)`), Streamable HTTP via `drsg serve` (local files off). `relay.rs`: walks up for `.mcp.json`, probes `/health`, pumps JSON-RPC between host stdio and the running `serve watch`.

**Logging** (`dr-strange-log/src/lib.rs:51-84`): stderr fmt layer + daily-rolling non-blocking file layer under `$DRSG_LOG_DIR`.

**Hooks.** `drsg-shell-guard` (PreToolUse/Bash, exit 2 on `rg/grep/cat/sed -n…` unless `DRSG_RAW=1`), `drsg-session-brief` (SessionStart), repo-level `drsg-ensure-server` and `drsg-usage-report`.

**CI/release/Docker/install.** `ci.yml`: fmt/clippy/test, feature-matrix, frontend, docs, five SDK e2e jobs. `release.yml`: five-target matrix, tar/zip + `.sha256`, GitHub Release, GHCR multi-arch. `install.sh`/`install.ps1` verify SHA-256 if the sidecar exists.

#### Code quality

Strengths: handler-as-function pattern with in-memory `Database` tests; shared logic between CLI/MCP/web; tool errors carry the full anyhow chain; tool gate tested for sharing across sessions (lib.rs:2616-2658); schema regression test for Gemini's boolean-schema quirk (lib.rs:2819); shell guard tested by executing the script (commands.rs:4720). No `unwrap/expect` on user input in non-test code. Gaps: no test for `installer_command` with `--bin`, none for `read_range`/`snippet` path containment, no end-to-end `relay_over` test, `drsg-usage-report.py` untested.

#### Concerns

Confirmed:

1. **Path traversal in MCP `snippet`** (lib.rs:1790, 1813, 1927-1950). `grep`'s filter is safe but follows symlinks out of the tree (lib.rs:983).
2. **Self-update trusts unpinned `master` script, no signature** (update.rs:490-491; install.sh:117-131; install.ps1:73-78). `--bin` interpolated unquoted (update.rs:681).
3. **Destructive MCP ops without confirmation.** `drop_plane` gated (lib.rs:2377), but `cypher` `DELETE/REMOVE/SET` (lib.rs:1420-1444), `write_nodes/write_edges`, `digest apply:true` are not.
4. **Secrets in files.** `.mcp.json`, `.cursor/mcp.json`, `.opencode.json`, `.gemini/settings.json` receive the literal token (commands.rs:690-766); only `.mcp.json` gitignored (commands.rs:107-114). `[llm]` keys become env vars visible to child processes (config.rs:261-263). `drsg.example.toml:15` ships `token = "change-me"`.
5. **Hook footguns.** `.claude/settings.json:9,20` runs `drsg init` with `DRSG_ENSURE_UNCONDITIONAL=1` on every session start; `upsert_claude_hook` repoints any hook whose command `ends_with(name)` (commands.rs:951-956); shell guard's `*'>'*` bypass (drsg-shell-guard:37).
6. **CI supply chain.** Actions pinned by floating major tags; `bun install` unfrozen (ci.yml:100, release.yml:64, Dockerfile:16).
7. **`init` respawn race.** `addr_bindable`/`pick_free_port` TOCTOU (commands.rs:572-584); `terminate` SIGTERMs a pid read from an HTTP body (commands.rs:174-188).

Suspected:

8. Tool results echo file contents and node properties verbatim; a poisoned repo file becomes trusted context.
9. `drsg-usage-report.py` writes watermarks to predictable `/tmp/drsg-usage-<id>.json` (`drsg_usage_report.py:99-112`).
10. `relay` forwards `Authorization` from any `.mcp.json` found walking up the tree (relay.rs:369-384).

#### Design decisions to flag

- Config-by-environment leaks every secret to all child processes.
- `update` = re-run the curl-pipe installer; a pinned tag or minisign check would close the gap.
- Message-level relay cannot enforce policy on a forwarded session.
- Tool gate queues rather than rejects; no per-call deadline.

### 2.7 SDKs, benchmarks, docs

#### Architecture

All six SDKs are built from `crates/dr-strange-web/openrpc.json` (36 methods, 10 schemas):

| Lang | Core | Codegen → output | Drift check |
|---|---|---|---|
| Python | `sdk/python/src/drsg/_client.py` (urllib) | `codegen.py` → `_generated.py` | `tests/test_generated.py` |
| TS | `src/client.ts` (fetch) | `codegen.mjs` → `src/generated.ts` | `test/generated.test.ts` |
| Go | `drsg.go` (net/http) | `internal/gen/gen.go` → `generated.go` | `generated_test.go` |
| Java | `Client.java` (JDK HttpClient + Jackson) | `codegen/Codegen.java` → `Drsg.java` | `GeneratedDriftTest` |
| C | `src/drsg.c` (libcurl + json-c) | `codegen/codegen.c` → `drsg_generated.{h,c}` | `make check-drift` |
| Zig | `src/drsg.zig` (`@cImport`s the C SDK) | none | none |

Transport: JSON-RPC 2.0 POST to `/rpc`; bearer from arg or `$DRSG_TOKEN`; `-32001` becomes a typed auth error everywhere. No SDK implements retry, including for retryable `-32002`. Change feed: every core except Zig has a `plane.watch` WebSocket client; only TS reconnects. Parity: typed results in TS/Go/Java; Python returns `Any`; C/Zig return raw `json_object`.

Packaging: nothing is published. `sdk/README.md:8-15` advertises `bun add drsg`, `go get`, Maven coordinates; no `sdk/go/vX` tags. Versions disagree: pom `2.0.0-alpha` vs Java README `0.1.0`; pyproject `2.0.0a0` vs `__version__ = "0.1.0"`; openrpc `0.1.0` vs workspace 2.7.0.

Tests: each SDK spawns a real `drsg serve`; CI runs five `sdk-*` jobs (ci.yml:129-236). Zig has no CI job.

Benchmarks: `drsg-bench gen` writes a deterministic dataset (100K nodes/500K edges/128-dim); `drsg-bench run` times drsg in-process; `compare.py` replays on SQLite, Kùzu, Neo4j; `aggregate.py` renders `BENCHMARKS.md`.

#### Concerns

C / Zig:
- Confirmed: server-controlled frame length unbounded — `malloc(len)` (drsg.c:304), `realloc` (drsg.c:331); same in Go `make([]byte, length)` (watch.go:247) and Python `_read(length)`.
- Confirmed: token interpolated into WS URL unescaped (drsg.c:415-425).
- Confirmed: `drsg_watch` cannot be cancelled except from the callback; `ensure_global_init` not thread-safe (drsg.c:20-26); one shared `CURL*`.
- Confirmed (latent): codegen fixed `req[16], opt[16]` with no bounds check (codegen.c:139,153-155); `plane.hybrid` already has 13 optionals.
- Suspected: fixed mask key / `Sec-WebSocket-Key` without validating `Sec-WebSocket-Accept` (drsg.c:243-244, 423).
- Zig quickstart unwraps `.?` on possibly-null results (quickstart.zig:41,49); `build.zig:9` hard-codes `/usr/include`.

Go: `Watch` leaks a goroutine when the server closes first (watch.go:84-87); dial/handshake ignore `ctx` (watch.go:142-144, 169).

Java: no `close()`; `HttpClient` per instance (Client.java:68); `Subscription.close()` never `abort()`s (222); `buildAsync(...).join()` unbounded (217); listener exceptions swallowed (206-208); fixed 30s timeout (67). Suspected: `ClientE2ETest` order-dependent (95).

Python: truncated frame raises `struct.error` (`_client.py:213-215`); `json.loads` failures escape as `ValueError` (86); per-byte `_xor` (256-257); no async client; no `py.typed`.

TypeScript: timeouts surface as generic "connection failed" (client.ts:133-135); `.d.ts` depends on `lib: DOM` + `types: ["bun"]`; `examples/quickstart.ts:3` imports `"drsg"` unresolvable in-repo.

Schema: `plugin.list/catalog/install/remove` and `plane.vectorize` carry no `x-access`; eight params omit `required`.

Benchmark methodology:
- drsg measured in-process (main.rs:363-366) vs Python-driver overhead for others (compare.py:124-127, 205-208).
- Neo4j `vector_build` includes inserting 100K vectors (compare.py:361-373); drsg's excludes loading (main.rs:423-444).
- Load I/O accounting differs across engines (compare.py:104-106, 303; main.rs:289-290).
- No recall measurement.
- Results JSON gitignored (benchmarks/.gitignore:2). Stale "redb" comments (main.rs:8,70).
- Suspected: Kùzu lookup result never iterated (compare.py:207).
- `AGENT-BENCHMARKS.md` has no scripts/prompts/ledgers despite `README.md:318`.

Docs vs code: `docs/en/src/sdk.md:71` "Every SDK can open a long-lived WebSocket" — Zig has none. Install commands describe unpublished packages.

---

## Appendix — `unsafe` inventory (17 sites)

| Location | Purpose | Assessment |
|---|---|---|
| `core/src/storage/vector.rs:100-319` (8) | AVX2 dot / L2 kernels | Runtime CPU detection, length-checked; sound |
| `llm/src/preprocess/wasm.rs:372` | `Component::deserialize` of pinned `.cwasm` | Trusts registry pin; same-user attacker could swap |
| `llm/src/preprocess/wasm.rs:527` | `transmute` `&dyn Host` → `&'static` | Sound while the `Store` stays local to one call; maintenance hazard |
| `cli/src/config.rs:249` | `set_var` before threads spawn | Sound per documented call order |
| `web/src/methods.rs:336-364` (4) | macOS/Windows process memory FFI | Zeroed structs, checked return codes |
| `log/tests/init.rs:13`, `web/tests/mcp.rs:52` | test-only `set_var` | Fine |
