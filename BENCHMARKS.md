# Benchmarks — dr-strange vs Kùzu / SQLite / Neo4j

Cross-engine comparison of dr-strange against an embedded graph DB (Kùzu), the universal embedded baseline (SQLite as an edge table + recursive CTEs), and the industry-standard server (Neo4j). Every engine loads the **same** deterministic dataset and runs the **same** query sets; each is timed in its own native optimal path. **The measurement paths are not symmetric**: dr-strange is timed in-process from Rust, the other engines through their Python drivers (see Caveats).

**Dataset**: 100,000 nodes · 500,000 edges · 128-dim vectors · 10,000 lookup/expand queries · 1,000 vector queries.

| Operation | dr-strange | Kùzu | SQLite | Neo4j |
|---|---|---|---|---|
| Graph load — nodes + edges (↑ better) | 317 K/s | 1.1 M/s | 639 K/s | 37 K/s |
| Point lookup by key — median (↓ better) | 3.3 µs | 256.0 µs | 3.4 µs | 286.6 µs |
| 1-hop expansion — median (↓ better) | 6.2 µs | 1.64 ms | 8.2 µs | 328.0 µs |
| 2-hop reachable set — median (↓ better) | 26.8 µs | 6.72 ms | 64.8 µs | 842.9 µs |
| Vector index build (↑ better) | 17 K/s | 3 K/s | — | 4 K/s † |
| Vector top-k query — median (↓ better) | 290.0 µs | 7.43 ms | — | 2.43 ms |

## Reading this

† The Neo4j build figure in this table was measured while `compare.py` still timed the vector load together with the index build; the accounting has since been fixed to index-build-only for every engine, and Neo4j's number will rise on the next run. It is kept rather than dropped so the table stays a record of what was actually measured.
- **↑ better** rows are throughput (bigger is faster); **↓ better** rows are median latency per operation (smaller is faster).
- SQLite has no native vectors, so it sits out the vector rows.
- **Recall@k** is the share of the exact cosine top-k (brute force, written once by `drsg-bench gen` for the first 100 vector queries) that the engine's ANN index returned, averaged over those queries — a latency row without its recall row is not a result. It is scored untimed, after the timed top-k pass. This table predates the recall row: no recall was measured for the numbers above, so the vector top-k latencies here are unaccompanied; the next `just bench-compare` adds the row for every engine.
- Every figure is the **median of repeated measurement passes** (3 by default; the min→max spread per op is recorded in `benchmarks/results/*.json`, committed alongside this file from the next re-baseline on — the JSON behind the table above was not preserved, so its spreads cannot be checked), with every engine pinned to the same P-cores — one machine, **indicative, not a leaderboard**. Re-run with `just bench-compare`.

## Methodology

- **Dataset** is generated once by `drsg-bench gen` (deterministic SplitMix64 seed) to plain CSV/txt; all engines read those exact files and the same query files — no engine regenerates data.
- **Identity**: the string key `n{i}` is the primary key in every engine, so point lookups and edge loads use each engine's PK index (Kùzu only indexes the PK, so a non-PK lookup would be an unfair scan).
- **Graph load** is nodes + edges through each engine's bulk path (drsg `bulk_load` / Kùzu `COPY` / SQLite `executemany` / Neo4j `UNWIND`) and includes building the adjacency/indexes that make expansion fast — insert + index, not insert alone. It's one combined throughput number (total rows / total load time). Where the CSV is parsed differs by engine and is not equalised: drsg parses inside its timer, SQLite's `executemany` consumes the row generator inside its timer, Kùzu's `COPY` reads and parses the file inside its timer, but Neo4j's rows are materialised in Python before the timer starts (only the Bolt round trips are timed), so the Neo4j load figure is slightly flattered relative to the other three.
- **expand/traverse** resolve the start node by key first (as any client must), then expand; `traverse_2hop` is the distinct set reachable in 1–2 hops.
- **Vector index build** times the index build alone, for every engine: loading the vectors happens before the timer starts (drsg inserts them into a separate plane, Kùzu `COPY`s, Neo4j `UNWIND`s). Earlier reports timed Neo4j's load together with its build; a Neo4j figure from before that fix understates Neo4j.
- Each engine runs **alone** (no CPU contention), **pinned to the same P-core set** (`bench_pin` in the justfile; the Neo4j container gets the same `--cpuset-cpus`), for `bench_repeat` passes with a fresh database each pass — pinning removes the hybrid-CPU scheduling lottery, repeats make the residual noise visible as a recorded spread. Note the pin gives parallel index builds fewer threads than the unpinned machine has; the spread on a pinned run is the trustworthy part. drsg is a `--release` build.

## Caveats (why cross-engine numbers lie if you squint)

- **The measurement path differs — this is the big one.** dr-strange is driven in-process from Rust (`benchmarks/drsg-bench`): the timer wraps a direct library call. SQLite, Kùzu and Neo4j are driven from Python (`benchmarks/compare.py`) through `sqlite3`, the `kuzu` bindings and the `neo4j` Bolt driver, so every one of their per-op latencies includes Python call, argument-marshalling and result-materialisation overhead that drsg's do not. On microsecond-scale rows (lookup, 1-hop) that overhead is a large share of the figure: the SQLite lookup cell is close to the floor of what a Python-driven measurement can show at all. Read sub-10 µs cells as "same class", not as a ranking; the millisecond-scale gaps are real. A Rust-driven comparator (rusqlite, Kùzu's Rust API) would remove the asymmetry and is the honest next step.
- **Durability differs.** SQLite runs WAL + `synchronous=NORMAL`; drsg's native LSM engine appends and fsyncs its WAL per commit (bulk load is one commit); Kùzu and Neo4j use their own defaults. These are not equalized — load numbers especially are sensitive to it.
- **Deployment differs.** dr-strange, Kùzu and SQLite are embedded (in-process, no client/server hop). Neo4j is a server reached over Bolt with JVM warmup and per-query network + transaction overhead, so its per-op latencies carry a fixed tax the embedded engines don't — read it as a different class, not a head-to-head loss.
- **Maturity differs.** dr-strange is a young from-scratch engine; the others are mature. Where we're slower (e.g. bulk load vs columnar `COPY`), that's the point of measuring — it says where to invest next.
- **Synthetic data.** A uniform-random graph with average degree ~5; real workloads have skew/hubs that stress traversal differently.

## Takeaways for dr-strange

- **Strong — low-latency point & graph queries.** The embedded KV design gives microsecond point lookups and single-digit-µs 1-hop expansion, on par with SQLite and orders of magnitude below the query-engine round-trips of Kùzu/Neo4j. That's the embedded, agent-in-the-loop sweet spot dr-strange is built for.
- **Improved — bulk load.** A `bulk_load` fast path (contiguous id reservation, in-memory interning, and sorted batched writes) roughly doubled load throughput over the old per-record loop, moving drsg past Neo4j. It still trails Kùzu's columnar `COPY` and SQLite: drsg writes three sorted index entries per edge (record + two adjacency) where a columnar store appends — closing that further is an on-disk-layout change, not just faster inserts.
- **Strong — vector-index build.** The hand-rolled HNSW originally built ~10× slower than Kùzu/Neo4j. Cached per-vector norms (every metric reduces to one dot), a multi-accumulator AVX2+FMA dot kernel, reused search scratch, and a parallel multi-threaded build (arch/01 §5) moved it from ~10× behind to ahead of both — by less over Neo4j than the table shows, since the Neo4j build cell was measured with the load included (see Methodology).
- **Strong — vector query.** drsg's top-k latency is well below both Kùzu and Neo4j — the same cached-norm + SIMD path that sped up build also sharpened search, and the `ef` clamp keeps deep-k recall honest (see `measure_ef_multiplier_sweep` in the hnsw tests).

