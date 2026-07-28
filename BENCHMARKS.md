# Benchmarks — dr-strange vs Kùzu / SQLite / Neo4j

Cross-engine comparison of dr-strange against an embedded graph DB (Kùzu), the universal embedded baseline (SQLite as an edge table + recursive CTEs), and the industry-standard server (Neo4j). Every engine loads the **same** deterministic dataset and runs the **same** query sets; each is timed in its own native optimal path.

**Dataset**: 100,000 nodes · 500,000 edges · 128-dim vectors · 10,000 lookup/expand queries · 1,000 vector queries.

| Operation | dr-strange | Kùzu | SQLite | Neo4j |
|---|---|---|---|---|
| Bulk-load nodes (↑ better) | 111 K/s | 278 K/s | 485 K/s | 32 K/s |
| Bulk-load edges (+ adjacency/index) (↑ better) | 87 K/s | 1.4 M/s | 609 K/s | 26 K/s |
| Point lookup by key — median (↓ better) | 3.3 µs | 365.7 µs | 4.5 µs | 978.6 µs |
| 1-hop expansion — median (↓ better) | 6.3 µs | 2.26 ms | 11.4 µs | 799.5 µs |
| 2-hop reachable set — median (↓ better) | 32.5 µs | 9.19 ms | 79.2 µs | 1.56 ms |
| Vector index build (↑ better) | 318/s | 3 K/s | — | 3 K/s |
| Vector top-k query — median (↓ better) | 1.18 ms | 9.99 ms | — | 3.57 ms |

## Reading this

- **↑ better** rows are throughput (bigger is faster); **↓ better** rows are median latency per operation (smaller is faster).
- SQLite has no native vectors, so it sits out the two vector rows.
- Numbers are single-run, warm, on one machine — **indicative, not a leaderboard**. Re-run with `just bench-compare`.

## Methodology

- **Dataset** is generated once by `drsg-bench gen` (deterministic SplitMix64 seed) to plain CSV/txt; all engines read those exact files and the same query files — no engine regenerates data.
- **Identity**: the string key `n{i}` is the primary key in every engine, so point lookups and edge loads use each engine's PK index (Kùzu only indexes the PK, so a non-PK lookup would be an unfair scan).
- **load_edges** includes building the structure that makes expansion fast (drsg adjacency tables / SQLite `src` index / Kùzu rel storage / Neo4j relationships), so it is edge-insert + index, not insert alone.
- **expand/traverse** resolve the start node by key first (as any client must), then expand; `traverse_2hop` is the distinct set reachable in 1–2 hops.
- Each engine runs **alone** (no CPU contention). drsg is a `--release` build.

## Caveats (why cross-engine numbers lie if you squint)

- **Durability differs.** SQLite runs WAL + `synchronous=NORMAL`; drsg uses redb's default durability; Kùzu and Neo4j use their own defaults. These are not equalized — load numbers especially are sensitive to it.
- **Deployment differs.** dr-strange, Kùzu and SQLite are embedded (in-process, no client/server hop). Neo4j is a server reached over Bolt with JVM warmup and per-query network + transaction overhead, so its per-op latencies carry a fixed tax the embedded engines don't — read it as a different class, not a head-to-head loss.
- **Maturity differs.** dr-strange is a from-scratch engine at M6; the others are mature. Where we're slower (e.g. vector-index build), that's the point of measuring — it says where to invest next.
- **Synthetic data.** A uniform-random graph with average degree ~5; real workloads have skew/hubs that stress traversal differently.

## Takeaways for dr-strange

- **Strong — low-latency point & graph queries.** The embedded KV design gives microsecond point lookups and single-digit-µs 1-hop expansion, on par with SQLite and orders of magnitude below the query-engine round-trips of Kùzu/Neo4j. That's the embedded, agent-in-the-loop sweet spot dr-strange is built for.
- **Weak — bulk edge load.** Kùzu's columnar `COPY` loads edges far faster; drsg writes each edge's two adjacency entries through redb one at a time. A bulk-load fast path (batched writes, deferred index maintenance) is the clearest throughput win.
- **Weak — vector-index build.** drsg's hand-rolled HNSW builds an order of magnitude slower than Kùzu's and Neo4j's. The deferred HNSW sidecar + parallel/build-time optimizations (arch/01 §5) are where to invest for the AI-native story.
- **Competitive — vector query.** Once built, drsg's top-k latency is lower than both Kùzu and Neo4j here: the query path is healthy, it's index *construction* that lags.

