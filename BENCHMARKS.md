# Benchmarks — dr-strange vs Kùzu / SQLite / Neo4j

Cross-engine comparison of dr-strange against an embedded graph DB (Kùzu), the universal embedded baseline (SQLite as an edge table + recursive CTEs), and the industry-standard server (Neo4j). Every engine loads the **same** deterministic dataset and runs the **same** query sets; each is timed in its own native optimal path.

**Dataset**: 100,000 nodes · 500,000 edges · 128-dim vectors · 10,000 lookup/expand queries · 1,000 vector queries.

| Operation | dr-strange | Kùzu | SQLite | Neo4j |
|---|---|---|---|---|
| Graph load — nodes + edges (↑ better) | 181 K/s | 797 K/s | 502 K/s | 27 K/s |
| Point lookup by key — median (↓ better) | 3.0 µs | 397.6 µs | 5.5 µs | 978.6 µs |
| 1-hop expansion — median (↓ better) | 6.4 µs | 2.37 ms | 13.7 µs | 799.5 µs |
| 2-hop reachable set — median (↓ better) | 29.4 µs | 9.84 ms | 94.7 µs | 1.56 ms |
| Vector index build (↑ better) | 1 K/s | 3 K/s | — | 3 K/s |
| Vector top-k query — median (↓ better) | 380.5 µs | 10.39 ms | — | 3.57 ms |

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

- **Durability differs.** SQLite runs WAL + `synchronous=NORMAL`; drsg uses redb's default durability; Kùzu and Neo4j use their own defaults. These are not equalized — load numbers especially are sensitive to it.
- **Deployment differs.** dr-strange, Kùzu and SQLite are embedded (in-process, no client/server hop). Neo4j is a server reached over Bolt with JVM warmup and per-query network + transaction overhead, so its per-op latencies carry a fixed tax the embedded engines don't — read it as a different class, not a head-to-head loss.
- **Maturity differs.** dr-strange is a from-scratch engine at M6; the others are mature. Where we're slower (e.g. vector-index build), that's the point of measuring — it says where to invest next.
- **Synthetic data.** A uniform-random graph with average degree ~5; real workloads have skew/hubs that stress traversal differently.

## Takeaways for dr-strange

- **Strong — low-latency point & graph queries.** The embedded KV design gives microsecond point lookups and single-digit-µs 1-hop expansion, on par with SQLite and orders of magnitude below the query-engine round-trips of Kùzu/Neo4j. That's the embedded, agent-in-the-loop sweet spot dr-strange is built for.
- **Improved — bulk load.** A `bulk_load` fast path (contiguous id reservation, in-memory interning, and sorted batched writes with each table opened once) roughly doubled load throughput over the old per-record loop, moving drsg past Neo4j. It still trails Kùzu's columnar `COPY` and SQLite: drsg writes three sorted B-tree entries per edge (record + two adjacency) where a columnar store appends — closing that further is an on-disk-layout change, not just faster inserts.
- **Improved — vector-index build.** The original hand-rolled HNSW built ~10× slower than Kùzu/Neo4j. After caching per-vector norms (so every metric is one dot), an AVX2+FMA dot kernel, and removing per-search allocation, build is ~4× faster and now within ~2× of the mature engines. The remaining gap is single-threaded construction; parallel build (deferred, arch/01 §5) would close it.
- **Strong — vector query.** drsg's top-k latency is now well below both Kùzu and Neo4j — the same cached-norm + SIMD path that sped up build also sharpened search.

