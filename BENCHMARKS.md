# Benchmarks — dr-strange vs Kùzu / SQLite / Neo4j

Cross-engine comparison of dr-strange against an embedded graph DB (Kùzu), the universal embedded baseline (SQLite as an edge table + recursive CTEs), and the industry-standard server (Neo4j). Every engine loads the **same** deterministic dataset and runs the **same** query sets; each is timed in its own native optimal path. **The measurement paths are not symmetric**: dr-strange is timed in-process from Rust, the other engines through their Python drivers (see Caveats).

**Dataset**: 100,000 nodes · 500,000 edges · 128-dim vectors · 10,000 lookup/expand queries · 1,000 vector queries.

| Operation | dr-strange | Kùzu | SQLite | Neo4j |
|---|---|---|---|---|
| Graph load — nodes + edges (↑ better) | 456 K/s | 1.4 M/s | 898 K/s | 37 K/s |
| Point lookup by key — median (↓ better) | 3.5 µs | 189.3 µs | 2.7 µs | 286.6 µs |
| 1-hop expansion — median (↓ better) | 5.8 µs | 1.26 ms | 6.4 µs | 328.0 µs |
| 2-hop reachable set — median (↓ better) | 26.0 µs | 5.12 ms | 48.1 µs | 842.9 µs |
| Vector index build (↑ better) | 22 K/s | 3 K/s | — | 4 K/s |
| Vector top-k query — median (↓ better) | 231.0 µs | 5.55 ms | — | 2.43 ms |
| Vector top-k recall@k (vs exact) (↑ better) | 0.278 @10 | — | — | — |

## Reading this

- **↑ better** rows are throughput (bigger is faster); **↓ better** rows are median latency per operation (smaller is faster).
- SQLite has no native vectors, so it sits out the vector rows.
- **Recall@k** is the share of the exact cosine top-k (brute force, written once by `drsg-bench gen` for the first 100 vector queries) that the engine's ANN index returned, averaged over those queries — a latency row without its recall row is not a result. It is scored untimed, after the timed top-k pass.
- Every figure is the **median of repeated measurement passes** (3 by default; the min→max spread per op is recorded in `benchmarks/results/*.json`, committed alongside this file), with every engine pinned to the same P-cores — one machine, **indicative, not a leaderboard**. Re-run with `just bench-compare`.
- **The columns are not all from one sitting** — dr-strange 2026-10-22, Kùzu 2026-10-22, SQLite 2026-10-22, Neo4j 2026-10-21. A column measured on another day saw a different machine, and that moves every figure in it; read across days as indicative only.

## Methodology

- **Dataset** is generated once by `drsg-bench gen` (deterministic SplitMix64 seed) to plain CSV/txt; all engines read those exact files and the same query files — no engine regenerates data.
- **Identity**: the string key `n{i}` is the primary key in every engine, so point lookups and edge loads use each engine's PK index (Kùzu only indexes the PK, so a non-PK lookup would be an unfair scan).
- **Graph load** is nodes + edges through each engine's bulk path (drsg `bulk_load` / Kùzu `COPY` / SQLite `executemany` / Neo4j `UNWIND`) and includes building the adjacency/indexes that make expansion fast — insert + index, not insert alone. It's one combined throughput number (total rows / total load time). Where the CSV is parsed differs by engine and is not equalised: drsg parses inside its timer, SQLite's `executemany` consumes the row generator inside its timer, Kùzu's `COPY` reads and parses the file inside its timer, but Neo4j's rows are materialised in Python before the timer starts (only the Bolt round trips are timed), so the Neo4j load figure is slightly flattered relative to the other three.
- **expand/traverse** resolve the start node by key first (as any client must), then expand; `traverse_2hop` is the distinct set reachable in 1–2 hops.
- **upsert** is drsg-only — no other engine here is driven through an equivalent — and is three rows over one batch, all built before the timer starts, so the only difference between them is what the engine does about a key that is already taken. `upsert-fresh` writes into an empty plane (every key new); `upsert-skip` and `upsert-update` re-offer the batch the `load` row already wrote, so every key collides. They run last in the pass: `upsert-update` rewrites every node's properties, and measured before the read rows it would be their noise rather than its own number. Not comparable with `load`, which also times parsing and edges — comparable with each other, which is the question a re-ingest asks.
- **Vector index build** times the index build alone, for every engine: loading the vectors happens before the timer starts (drsg inserts them into a separate plane, Kùzu `COPY`s, Neo4j `UNWIND`s). Earlier reports timed Neo4j's load together with its build; a Neo4j figure from before that fix understates Neo4j.
- Each engine runs **alone** (no CPU contention), **pinned to the same P-core set** (`bench_pin` in the justfile; the Neo4j container gets the same `--cpuset-cpus`), for `bench_repeat` passes with a fresh database each pass — pinning removes the hybrid-CPU scheduling lottery, repeats make the residual noise visible as a recorded spread. Note the pin gives parallel index builds fewer threads than the unpinned machine has; the spread on a pinned run is the trustworthy part. drsg is a `--release` build.

## Caveats (why cross-engine numbers lie if you squint)

- **The measurement path differs — this is the big one.** dr-strange is driven in-process from Rust (`benchmarks/drsg-bench`): the timer wraps a direct library call. SQLite, Kùzu and Neo4j are driven from Python (`benchmarks/compare.py`) through `sqlite3`, the `kuzu` bindings and the `neo4j` Bolt driver, so every one of their per-op latencies includes Python call, argument-marshalling and result-materialisation overhead that drsg's do not. On microsecond-scale rows (lookup, 1-hop) that overhead is a large share of the figure: the SQLite lookup cell is close to the floor of what a Python-driven measurement can show at all. Read sub-10 µs cells as "same class", not as a ranking; the millisecond-scale gaps are real. A Rust-driven comparator (rusqlite, Kùzu's Rust API) would remove the asymmetry and is the honest next step.
- **Durability differs.** SQLite runs WAL + `synchronous=NORMAL`; drsg's native LSM engine appends and fsyncs its WAL per commit (bulk load is one commit); Kùzu and Neo4j use their own defaults. These are not equalized — load numbers especially are sensitive to it.
- **Deployment differs.** dr-strange, Kùzu and SQLite are embedded (in-process, no client/server hop). Neo4j is a server reached over Bolt with JVM warmup and per-query network + transaction overhead, so its per-op latencies carry a fixed tax the embedded engines don't — read it as a different class, not a head-to-head loss.
- **Maturity differs.** dr-strange is a young from-scratch engine; the others are mature. Where we're slower (e.g. bulk load vs columnar `COPY`), that's the point of measuring — it says where to invest next.
- **Synthetic data.** A uniform-random graph with average degree ~5; real workloads have skew/hubs that stress traversal differently.
- **Machine state dominates these numbers.** Every figure needs an otherwise idle machine. Re-measured with a browser and a screen recorder running, the same binaries came out 30–50% worse on every operation — read paths included, which no engine change could explain — and the load spread widened from ±13% to ±42%. A pass taken while the desktop is in use is not a measurement; check the machine is quiet before believing a re-run.

## Digesting a repository

What it costs to turn a working tree into a graph, measured on this repository
(97,308 lines of code, 355 files read after `.gitignore` withheld 377 of 732),
with all thirteen catalog plugins installed:

| | |
|---|---|
| Wall time | **3.1 s** (3.06 / 3.13 / 3.27 over three runs) |
| Result | 10,237 nodes, 44,927 edges, plus 1,440 history nodes |
| On disk | 27 MB |
| Model calls | none |

Measured with `drsg digest <path> --apply --code-only` on an i9-14900HX,
unpinned, each run against a **fresh** database — which is what a first run
costs. Re-digesting into a database whose planes were dropped is slower (10–15 s
here) because the dropped space is not reclaimed until compaction; that is a
different measurement, not this one.

`--code-only` matters: without it the run also routes files no plugin claims
(`.json`, `.sh`, `.svelte`, …) to the document path, which needs an LLM key and
fails without one. Nothing in the figures above involves a model.

## Answering a code question — graph vs text search

The question is the one an agent asks before changing anything: *what calls this
symbol, and what would a change reach?* Two ways to answer it about the same
symbols, counted in tiktoken `cl100k_base` tokens, not estimated:

- **graph** — one `drsg context <key>` call.
- **text** — `rg -nw <name>`, then reading every file that matched, which is
  what an agent without a graph must do to see the call sites.

| | graph | text |
|---|---|---|
| Median tokens | **925** | **53,720** |
| Median ratio | — | **61× more** |
| Range over the sample | — | 7× – 473× |

n = 12, selected by rule rather than by hand: functions this repository defines
with a recorded in-degree of 5–40 — the ones someone might actually be about to
change — sorted by key and sampled at a fixed stride. Reproduce with
`benchmarks/token_cost.py`, which prints the per-symbol table.

### Why the sample excludes some symbols

Three exclusions, each of which *lowers* the ratio, and without which this
number would be dishonest:

- **Test functions.** Nobody asks what calls a test.
- **Language-generic names** — `new`, `open`, `run` and the like. The text
  baseline would have to grep that bare word; `new` matches 206 files here, and
  the ratio for such symbols reaches 2,600×. No developer searches that way, so
  counting it would be measuring a straw man.
- **Names under five characters**, for the same reason.

What remains is the case text search handles *well*: a distinctive identifier,
grepped with word boundaries, finding every real call site plus some noise.

### Caveats

- **The two answers are not identical.** `context` returns resolved edges — it
  knows which `parse` is meant; grep returns every lexical match and leaves the
  reading to the agent. Where a call is unresolved (dynamic dispatch, untyped
  receivers) the graph omits it and says so, while grep would have found the
  text. The comparison is of cost to reach a usable answer, not of recall.
- **Whole-file reads are the charitable baseline.** An agent that reads only
  the matching hunk would spend less; one that reads a file per hit and then
  follows callers would spend more. Reading each matched file once is the
  middle of that range.
- **One repository.** A codebase with shorter files, or one where names are
  less distinctive, will give a different ratio. Run the script on yours.

## Takeaways for dr-strange

- **Strong — low-latency point & graph queries.** The embedded KV design gives microsecond point lookups and single-digit-µs 1-hop expansion, on par with SQLite and orders of magnitude below the query-engine round-trips of Kùzu/Neo4j. That's the embedded, agent-in-the-loop sweet spot dr-strange is built for.
- **Improved — bulk load.** A `bulk_load` fast path (contiguous id reservation, in-memory interning, and sorted batched writes) roughly doubled load throughput over the old per-record loop, moving drsg past Neo4j. It still trails Kùzu's columnar `COPY` and SQLite: drsg writes three sorted index entries per edge (record + two adjacency) where a columnar store appends — closing that further is an on-disk-layout change, not just faster inserts. Two later changes — handing owned buffers to the write batch instead of copying them, and a slice-by-8 CRC in place of one byte per round — added a further **24%** (343 → 426 K rows/s), measured as an interleaved old/new A/B in one window so the delta is the code rather than the hour of day.
- **Known — a re-ingest costs what it writes.** Re-offering a batch the plane
  already holds is three different prices. Skipping is the cheapest of the
  three — the same point lookup per key a fresh insert already pays, and then
  nothing written — though its margin over inserting is inside the noise on
  anything but a quiet machine, so read it as the same class rather than as a
  win. Updating is roughly **4× slower
  than a fresh insert** — the same lookups, plus a read-modify-write per node
  through the property path, which is where index membership and the change
  feed are maintained and so cannot be batched the way the loader's writes
  are. The ratio is the trustworthy part here: all three rows are measured in
  one pass on one machine, so it survives the machine noise the absolute
  figures do not. Fixing it would mean a batched property-update path, which
  is an engine change and not yet worth it — a re-ingest is rarer than a load.

- **Strong — vector-index build.** The hand-rolled HNSW originally built ~10× slower than Kùzu/Neo4j. Cached per-vector norms (every metric reduces to one dot), a multi-accumulator AVX2+FMA dot kernel, reused search scratch, and a parallel multi-threaded build (arch/01 §5) moved it from ~10× behind to ahead of both — by less over Neo4j than the table shows, since the Neo4j build cell was measured with the load included (see Methodology).
- **Strong — vector query.** drsg's top-k latency is well below both Kùzu and Neo4j — the same cached-norm + SIMD path that sped up build also sharpened search, and the `ef` clamp keeps deep-k recall honest (see `measure_ef_multiplier_sweep` in the hnsw tests).

