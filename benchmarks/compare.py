# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "kuzu>=0.7",
#   "neo4j>=5.20",
# ]
# ///
"""Cross-engine side of the dr-strange benchmark (arch/00 §5 / M5 deferred).

Runs the *same* workload as `drsg-bench run` against SQLite, Kùzu, and Neo4j,
reading the *same* dataset + query files that `drsg-bench gen` produced, and
emitting results JSON in the same schema. One engine per invocation:

    uv run benchmarks/compare.py --engine sqlite --data benchmarks/data \\
        --out benchmarks/results/sqlite.json

SQLite has no native vectors, so it sits out the vector rows. Kùzu and Neo4j
build a vector index and run top-k. Deployment/durability differences are real
and are called out in BENCHMARKS.md — treat cross-engine numbers as indicative,
not a leaderboard.
"""

import argparse
import json
import sys
import time
from pathlib import Path

# ---- shared helpers -------------------------------------------------------

def read_lines(path):
    return path.read_text().splitlines()


def load_meta(data):
    return json.loads((data / "meta.json").read_text())


def op_throughput(engine, op, n, total_ms):
    return {
        "engine": engine, "op": op, "n": n, "total_ms": total_ms,
        "throughput_per_s": n / (total_ms / 1000) if total_ms > 0 else 0.0,
    }


def op_latency(engine, op, micros, wall_s):
    s = sorted(micros)
    median = s[len(s) // 2]
    p95 = s[min(len(s) - 1, int(len(s) * 0.95))]
    return {
        "engine": engine, "op": op, "n": len(micros),
        "total_ms": wall_s * 1000,
        "median_us": median, "p95_us": p95,
        "throughput_per_s": len(micros) / wall_s if wall_s > 0 else 0.0,
    }


def node_rows(data):
    """Yield (key, label, name, value) from nodes.csv (skips id + header)."""
    lines = read_lines(data / "nodes.csv")
    for line in lines[1:]:
        _id, key, label, name, value = line.split(",", 4)
        yield key, label, name, int(value)


def edge_rows(data):
    """Yield (src_key, dst_key, type) from edges.csv."""
    lines = read_lines(data / "edges.csv")
    for line in lines[1:]:
        src, dst, ty = line.split(",", 2)
        yield src, dst, ty


def vector_rows(data):
    """Yield (key, [floats]) from vectors.csv."""
    lines = read_lines(data / "vectors.csv")
    for line in lines[1:]:
        ids, vec = line.split(",", 1)
        yield f"v{ids}", [float(x) for x in vec.split()]


def query_vectors(data):
    return [[float(x) for x in line.split()]
            for line in read_lines(data / "queries/vector_queries.csv")]


# ---- SQLite ---------------------------------------------------------------

def run_sqlite(data, out_path, k):
    import sqlite3

    engine = "sqlite"
    results = []
    dbfile = data / "sqlite.db"
    dbfile.unlink(missing_ok=True)
    con = sqlite3.connect(dbfile)
    # A reasonable durable config (documented in BENCHMARKS.md).
    con.execute("PRAGMA journal_mode=WAL")
    con.execute("PRAGMA synchronous=NORMAL")

    # load nodes: key is PRIMARY KEY (indexed).
    con.execute("CREATE TABLE nodes(key TEXT PRIMARY KEY, label TEXT, name TEXT, value INTEGER)")
    n_nodes = sum(1 for _ in node_rows(data))
    t0 = time.perf_counter()
    con.execute("BEGIN")
    con.executemany("INSERT INTO nodes VALUES (?,?,?,?)", node_rows(data))
    con.execute("COMMIT")
    results.append(op_throughput(engine, "load_nodes", n_nodes, (time.perf_counter() - t0) * 1000))

    # load edges + build the src index (analogous to drsg building adjacency).
    con.execute("CREATE TABLE edges(src TEXT, dst TEXT, type TEXT)")
    n_edges = sum(1 for _ in edge_rows(data))
    t0 = time.perf_counter()
    con.execute("BEGIN")
    con.executemany("INSERT INTO edges VALUES (?,?,?)", edge_rows(data))
    con.execute("COMMIT")
    con.execute("CREATE INDEX idx_src ON edges(src)")
    results.append(op_throughput(engine, "load_edges", n_edges, (time.perf_counter() - t0) * 1000))

    # point lookup by PK
    keys = read_lines(data / "queries/lookup_keys.txt")
    micros = []
    t0 = time.perf_counter()
    for key in keys:
        s = time.perf_counter()
        con.execute("SELECT label,name,value FROM nodes WHERE key=?", (key,)).fetchone()
        micros.append((time.perf_counter() - s) * 1e6)
    results.append(op_latency(engine, "lookup", micros, time.perf_counter() - t0))

    # 1-hop expansion
    ekeys = read_lines(data / "queries/expand_keys.txt")
    micros = []
    t0 = time.perf_counter()
    for key in ekeys:
        s = time.perf_counter()
        con.execute("SELECT dst FROM edges WHERE src=?", (key,)).fetchall()
        micros.append((time.perf_counter() - s) * 1e6)
    results.append(op_latency(engine, "expand_1hop", micros, time.perf_counter() - t0))

    # 2-hop distinct reachable set (recursive CTE, depth ≤ 2)
    cte = """
        WITH RECURSIVE reach(n, depth) AS (
            SELECT ?, 0
            UNION
            SELECT e.dst, reach.depth + 1 FROM edges e
            JOIN reach ON e.src = reach.n WHERE reach.depth < 2
        )
        SELECT DISTINCT n FROM reach WHERE depth > 0
    """
    micros = []
    t0 = time.perf_counter()
    for key in ekeys[:2000]:
        s = time.perf_counter()
        con.execute(cte, (key,)).fetchall()
        micros.append((time.perf_counter() - s) * 1e6)
    results.append(op_latency(engine, "traverse_2hop", micros, time.perf_counter() - t0))

    con.close()
    write_results(out_path, results)


# ---- Kùzu -----------------------------------------------------------------

def run_kuzu(data, out_path, k):
    import shutil

    import kuzu

    engine = "kuzu"
    results = []
    dbdir = data / "kuzu_db"
    # Kùzu may store the DB as a single file or a directory depending on
    # version; also clear its WAL. Remove whatever's there for a clean load.
    for p in [dbdir, Path(str(dbdir) + ".wal")]:
        if p.is_dir():
            shutil.rmtree(p)
        elif p.exists():
            p.unlink()
    db = kuzu.Database(str(dbdir))
    con = kuzu.Connection(db)

    # schema: key is PRIMARY KEY (Kùzu indexes the PK).
    con.execute("CREATE NODE TABLE Node(id INT64, key STRING, label STRING, name STRING, value INT64, PRIMARY KEY(key))")
    con.execute("CREATE REL TABLE Edge(FROM Node TO Node, type STRING)")

    n_nodes = sum(1 for _ in node_rows(data))
    n_edges = sum(1 for _ in edge_rows(data))

    t0 = time.perf_counter()
    con.execute(f'COPY Node FROM "{data / "nodes.csv"}" (HEADER=true)')
    results.append(op_throughput(engine, "load_nodes", n_nodes, (time.perf_counter() - t0) * 1000))

    t0 = time.perf_counter()
    con.execute(f'COPY Edge FROM "{data / "edges.csv"}" (HEADER=true)')
    results.append(op_throughput(engine, "load_edges", n_edges, (time.perf_counter() - t0) * 1000))

    # Kùzu's idiomatic API is a direct execute() per query (it caches plans
    # internally; separate prepare+execute is deprecated). The per-call cost is
    # Kùzu's real point-query latency — it's a columnar/analytical engine, not
    # tuned for single-row lookups the way an embedded KV is.
    keys = read_lines(data / "queries/lookup_keys.txt")
    q_lookup = "MATCH (n:Node {key:$k}) RETURN n.label, n.name, n.value"
    micros = []
    t0 = time.perf_counter()
    for key in keys:
        s = time.perf_counter()
        con.execute(q_lookup, {"k": key})
        micros.append((time.perf_counter() - s) * 1e6)
    results.append(op_latency(engine, "lookup", micros, time.perf_counter() - t0))

    ekeys = read_lines(data / "queries/expand_keys.txt")
    q_expand = "MATCH (n:Node {key:$k})-[:Edge]->(m) RETURN m.key"
    micros = []
    t0 = time.perf_counter()
    for key in ekeys:
        s = time.perf_counter()
        res = con.execute(q_expand, {"k": key})
        while res.has_next():
            res.get_next()
        micros.append((time.perf_counter() - s) * 1e6)
    results.append(op_latency(engine, "expand_1hop", micros, time.perf_counter() - t0))

    q_traverse = "MATCH (n:Node {key:$k})-[:Edge*1..2]->(m) RETURN DISTINCT m.key"
    micros = []
    t0 = time.perf_counter()
    for key in ekeys[:2000]:
        s = time.perf_counter()
        res = con.execute(q_traverse, {"k": key})
        while res.has_next():
            res.get_next()
        micros.append((time.perf_counter() - s) * 1e6)
    results.append(op_latency(engine, "traverse_2hop", micros, time.perf_counter() - t0))

    # vectors — best-effort; if the extension/API isn't available, skip the
    # vector rows but keep the graph results.
    try:
        run_kuzu_vectors(con, data, k, results, engine)
    except Exception as e:  # noqa: BLE001
        print(f"kuzu: vector benchmark skipped ({e})", file=sys.stderr)

    write_results(out_path, results)


def run_kuzu_vectors(con, data, k, results, engine):
    import kuzu  # noqa: F401

    con.execute("INSTALL vector")
    con.execute("LOAD vector")
    meta = load_meta(data)
    dim = meta["dim"]
    con.execute(f"CREATE NODE TABLE Item(key STRING, emb FLOAT[{dim}], PRIMARY KEY(key))")

    # Bulk load embeddings via a bracketed-array temp CSV Kùzu can COPY.
    tmp = data / "vectors_kuzu.csv"
    with tmp.open("w") as f:
        for key, vec in vector_rows(data):
            f.write(f'{key},"[{",".join(f"{x:.6f}" for x in vec)}]"\n')
    n_vecs = sum(1 for _ in vector_rows(data))
    con.execute(f'COPY Item FROM "{tmp}" (HEADER=false)')

    t0 = time.perf_counter()
    con.execute("CALL CREATE_VECTOR_INDEX('Item', 'emb_idx', 'emb', metric := 'cosine')")
    results.append(op_throughput(engine, "vector_build", n_vecs, (time.perf_counter() - t0) * 1000))

    q_vec = "CALL QUERY_VECTOR_INDEX('Item', 'emb_idx', $q, $k) RETURN node.key ORDER BY distance"
    micros = []
    t0 = time.perf_counter()
    for q in query_vectors(data):
        s = time.perf_counter()
        res = con.execute(q_vec, {"q": q, "k": k})
        while res.has_next():
            res.get_next()
        micros.append((time.perf_counter() - s) * 1e6)
    results.append(op_latency(engine, "vector_topk", micros, time.perf_counter() - t0))


# ---- Neo4j ----------------------------------------------------------------

def run_neo4j(data, out_path, k, uri, user, password):
    from neo4j import GraphDatabase

    engine = "neo4j"
    results = []
    driver = GraphDatabase.driver(uri, auth=(user, password))
    driver.verify_connectivity()

    with driver.session() as ses:
        ses.run("MATCH (n) DETACH DELETE n")  # clean slate
        for idx in ses.run("SHOW INDEXES YIELD name RETURN name"):
            try:
                ses.run(f"DROP INDEX {idx['name']}")
            except Exception:  # noqa: BLE001, S110 — best-effort cleanup of pre-existing indexes
                pass
        ses.run("CREATE CONSTRAINT node_key IF NOT EXISTS FOR (n:Node) REQUIRE n.key IS UNIQUE")

        nodes = list(node_rows(data))
        t0 = time.perf_counter()
        for batch in chunks(nodes, 10000):
            ses.run(
                "UNWIND $rows AS r CREATE (n:Node {key:r.key, label:r.label, name:r.name, value:r.value})",
                rows=[{"key": k_, "label": l_, "name": nm, "value": v} for (k_, l_, nm, v) in batch],
            )
        results.append(op_throughput(engine, "load_nodes", len(nodes), (time.perf_counter() - t0) * 1000))

        edges = list(edge_rows(data))
        t0 = time.perf_counter()
        for batch in chunks(edges, 10000):
            ses.run(
                "UNWIND $rows AS r MATCH (a:Node {key:r.src}), (b:Node {key:r.dst}) "
                "CREATE (a)-[:EDGE {type:r.type}]->(b)",
                rows=[{"src": s_, "dst": d_, "type": t_} for (s_, d_, t_) in batch],
            )
        results.append(op_throughput(engine, "load_edges", len(edges), (time.perf_counter() - t0) * 1000))

        keys = read_lines(data / "queries/lookup_keys.txt")
        micros = []
        t0 = time.perf_counter()
        for key in keys:
            s = time.perf_counter()
            list(ses.run("MATCH (n:Node {key:$k}) RETURN n.label, n.name, n.value", k=key))
            micros.append((time.perf_counter() - s) * 1e6)
        results.append(op_latency(engine, "lookup", micros, time.perf_counter() - t0))

        ekeys = read_lines(data / "queries/expand_keys.txt")
        micros = []
        t0 = time.perf_counter()
        for key in ekeys:
            s = time.perf_counter()
            list(ses.run("MATCH (n:Node {key:$k})-[:EDGE]->(m) RETURN m.key", k=key))
            micros.append((time.perf_counter() - s) * 1e6)
        results.append(op_latency(engine, "expand_1hop", micros, time.perf_counter() - t0))

        micros = []
        t0 = time.perf_counter()
        for key in ekeys[:2000]:
            s = time.perf_counter()
            list(ses.run("MATCH (n:Node {key:$k})-[:EDGE*1..2]->(m) RETURN DISTINCT m.key", k=key))
            micros.append((time.perf_counter() - s) * 1e6)
        results.append(op_latency(engine, "traverse_2hop", micros, time.perf_counter() - t0))

        try:
            run_neo4j_vectors(ses, data, k, results, engine)
        except Exception as e:  # noqa: BLE001
            print(f"neo4j: vector benchmark skipped ({e})", file=sys.stderr)

    driver.close()
    write_results(out_path, results)


def run_neo4j_vectors(ses, data, k, results, engine):
    meta = load_meta(data)
    dim = meta["dim"]
    vecs = list(vector_rows(data))
    t0 = time.perf_counter()
    for batch in chunks(vecs, 5000):
        ses.run(
            "UNWIND $rows AS r CREATE (i:Item {key:r.key, emb:r.emb})",
            rows=[{"key": key, "emb": emb} for (key, emb) in batch],
        )
    ses.run(
        "CREATE VECTOR INDEX item_emb IF NOT EXISTS FOR (i:Item) ON i.emb "
        "OPTIONS {indexConfig: {`vector.dimensions`: $dim, `vector.similarity_function`: 'cosine'}}",
        dim=dim,
    )
    ses.run("CALL db.awaitIndexes()")
    results.append(op_throughput(engine, "vector_build", len(vecs), (time.perf_counter() - t0) * 1000))

    micros = []
    t0 = time.perf_counter()
    for q in query_vectors(data):
        s = time.perf_counter()
        list(ses.run(
            "CALL db.index.vector.queryNodes('item_emb', $k, $q) YIELD node RETURN node.key",
            k=k, q=q,
        ))
        micros.append((time.perf_counter() - s) * 1e6)
    results.append(op_latency(engine, "vector_topk", micros, time.perf_counter() - t0))


def chunks(seq, n):
    for i in range(0, len(seq), n):
        yield seq[i:i + n]


# ---- main -----------------------------------------------------------------

def write_results(out_path, results):
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(results, indent=2))
    print(f"wrote {len(results)} results → {out_path}")
    for r in results:
        if "median_us" in r:
            print(f"  {r['op']:<14} n={r['n']:<7} {r['total_ms']:>9.2f} ms  "
                  f"median {r['median_us']:>8.2f} µs  {r['throughput_per_s']:>12.0f}/s")
        else:
            print(f"  {r['op']:<14} n={r['n']:<7} {r['total_ms']:>9.2f} ms  "
                  f"{r['throughput_per_s']:>12.0f}/s")


def main():
    ap = argparse.ArgumentParser(description="dr-strange cross-engine benchmark driver")
    ap.add_argument("--engine", required=True, choices=["sqlite", "kuzu", "neo4j"])
    ap.add_argument("--data", type=Path, default=Path("benchmarks/data"))
    ap.add_argument("--out", type=Path)
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--neo4j-uri", default="bolt://localhost:7687")
    ap.add_argument("--neo4j-user", default="neo4j")
    ap.add_argument("--neo4j-password", default="benchpass")
    args = ap.parse_args()

    out = args.out or Path(f"benchmarks/results/{args.engine}.json")
    if args.engine == "sqlite":
        run_sqlite(args.data, out, args.k)
    elif args.engine == "kuzu":
        run_kuzu(args.data, out, args.k)
    elif args.engine == "neo4j":
        run_neo4j(args.data, out, args.k, args.neo4j_uri, args.neo4j_user, args.neo4j_password)


if __name__ == "__main__":
    main()
