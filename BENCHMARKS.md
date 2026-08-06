# Benchmarks — dr-strange vs Kùzu / SQLite / Neo4j

Cross-engine comparison of dr-strange against an embedded graph DB (Kùzu), the universal embedded baseline (SQLite as an edge table + recursive CTEs), and the industry-standard server (Neo4j). Every engine loads the **same** deterministic dataset and runs the **same** query sets; each is timed in its own native optimal path.

**Dataset**: 100,000 nodes · 500,000 edges · 128-dim vectors · 10,000 lookup/expand queries · 1,000 vector queries.

| Operation | dr-strange | Kùzu | SQLite | Neo4j |
|---|---|---|---|---|
| Graph load — nodes + edges (↑ better) | 286 K/s | 951 K/s | 660 K/s | 31 K/s |
| Point lookup by key — median (↓ better) | 12.0 µs | 306.8 µs | 3.8 µs | 783.0 µs |
| 1-hop expansion — median (↓ better) | 17.7 µs | 2.03 ms | 9.0 µs | 504.5 µs |
| 2-hop reachable set — median (↓ better) | 53.2 µs | 8.94 ms | 69.5 µs | 1.08 ms |
| Vector index build (↑ better) | 22 K/s | 3 K/s | — | 4 K/s |
| Vector top-k query — median (↓ better) | 320.5 µs | 9.28 ms | — | 3.51 ms |

## Reading this

- **↑ better** rows are throughput (bigger is faster); **↓ better** rows are median latency per operation (smaller is faster).
- SQLite has no native vectors, so it sits out the two vector rows.
- Numbers are single-run, warm, on one machine — **indicative, not a leaderboard**. Re-run with `just bench-compare`.

## Methodology

- **Dataset** is generated once by `drsg-bench gen` (deterministic SplitMix64 seed) to plain CSV/txt; all engines read those exact files and the same query files — no engine regenerates data.
- **Identity**: the string key `n{i}` is the primary key in every engine, so point lookups and edge loads use each engine's PK index (Kùzu only indexes the PK, so a non-PK lookup would be an unfair scan).
- **Graph load** is nodes + edges through each engine's bulk path (drsg `bulk_load` / Kùzu `COPY` / SQLite `executemany` / Neo4j `UNWIND`) and includes building the adjacency/indexes that make expansion fast — insert + index, not insert alone. It's one combined throughput number (total rows / total load time).
- **expand/traverse** resolve the start node by key first (as any client must), then expand; `traverse_2hop` is the distinct set reachable in 1–2 hops.
- Each engine runs **alone** (no CPU contention). drsg is a `--release` build.

## Caveats (why cross-engine numbers lie if you squint)

- **Durability differs.** SQLite runs WAL + `synchronous=NORMAL`; drsg's native LSM engine appends and fsyncs its WAL per commit (bulk load is one commit); Kùzu and Neo4j use their own defaults. These are not equalized — load numbers especially are sensitive to it.
- **Deployment differs.** dr-strange, Kùzu and SQLite are embedded (in-process, no client/server hop). Neo4j is a server reached over Bolt with JVM warmup and per-query network + transaction overhead, so its per-op latencies carry a fixed tax the embedded engines don't — read it as a different class, not a head-to-head loss.
- **Maturity differs.** dr-strange is a young from-scratch engine; the others are mature. Where we're slower (e.g. bulk load vs columnar `COPY`), that's the point of measuring — it says where to invest next.
- **Synthetic data.** A uniform-random graph with average degree ~5; real workloads have skew/hubs that stress traversal differently.

## Takeaways for dr-strange

- **Strong — low-latency point & graph queries.** The embedded KV design gives microsecond point lookups and single-digit-µs 1-hop expansion, on par with SQLite and orders of magnitude below the query-engine round-trips of Kùzu/Neo4j. That's the embedded, agent-in-the-loop sweet spot dr-strange is built for.
- **Improved — bulk load.** A `bulk_load` fast path (contiguous id reservation, in-memory interning, and sorted batched writes) roughly doubled load throughput over the old per-record loop, moving drsg past Neo4j. It still trails Kùzu's columnar `COPY` and SQLite: drsg writes three sorted index entries per edge (record + two adjacency) where a columnar store appends — closing that further is an on-disk-layout change, not just faster inserts.
- **Strong — vector-index build.** The hand-rolled HNSW originally built ~10× slower than Kùzu/Neo4j. Cached per-vector norms (every metric reduces to one dot), a multi-accumulator AVX2+FMA dot kernel, reused search scratch, and a parallel multi-threaded build (arch/01 §5) moved it from ~10× behind to several times ahead of both.
- **Strong — vector query.** drsg's top-k latency is well below both Kùzu and Neo4j — the same cached-norm + SIMD path that sped up build also sharpened search, and the `ef` clamp keeps deep-k recall honest (see `measure_ef_multiplier_sweep` in the hnsw tests).

