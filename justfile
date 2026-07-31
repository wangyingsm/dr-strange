# dr-strange task runner. `just --list` to see recipes.

# Build the web dashboard SPA (bun + Vite). Its output in
# crates/dr-strange-web/frontend/dist is embedded into the drsg binary at
# compile time, so run this before `cargo build` to ship the real UI (a
# placeholder page is embedded otherwise — see crates/dr-strange-web/build.rs).
web-build:
    cd crates/dr-strange-web/frontend && bun install && bun run build

# Run the SPA dev server (hot reload); proxies /rpc and /ws to a locally
# running `drsg serve` on port 7700.
web-dev:
    cd crates/dr-strange-web/frontend && bun install && bun run dev

# Serve a database with the dashboard + JSON-RPC API (default graph.drsg).
# Uses the native LSM backend (the default). The db path is a directory here
# (WAL + SSTs live inside it).
serve db="graph.drsg":
    cargo run -p dr-strange-cli -- --db {{db}} serve

# Same, but on the legacy redb backend (native LSM not compiled in). A redb db
# is a single file, so don't point it at a native db's directory path.
serve-redb db="graph.redb":
    cargo run -p dr-strange-cli --no-default-features --features redb-backend,digest -- --db {{db}} serve

# ---- benchmarks (see BENCHMARKS.md) --------------------------------------
# Cross-engine comparison vs Kùzu / SQLite / Neo4j. The dataset is generated
# once; every engine reads the same files. Results land in benchmarks/results/
# and are aggregated into BENCHMARKS.md.

# Generate the shared dataset (deterministic).
bench-gen nodes="100000" edges="500000" dim="128":
    cargo build --release -p drsg-bench
    ./target/release/drsg-bench gen --out benchmarks/data --nodes {{nodes}} --edges {{edges}} --dim {{dim}}

bench-drsg:
    cargo build --release -p drsg-bench
    ./target/release/drsg-bench run --data benchmarks/data --db benchmarks/data/drsg.redb --out benchmarks/results/dr-strange.json

bench-sqlite:
    uv run --no-project benchmarks/compare.py --engine sqlite

bench-kuzu:
    uv run --no-project benchmarks/compare.py --engine kuzu

# Neo4j needs a running server; start/stop it around the run.
bench-neo4j-up:
    docker run -d --name drsg-neo4j -p 7687:7687 -p 7474:7474 \
      -e NEO4J_AUTH=neo4j/benchpass -e NEO4J_server_memory_heap_max__size=2G \
      -e NEO4J_server_memory_pagecache_size=1G neo4j:5.26
    @echo "waiting for neo4j..." && sleep 15

bench-neo4j-down:
    -docker rm -f drsg-neo4j

bench-neo4j:
    uv run --no-project benchmarks/compare.py --engine neo4j

# Aggregate whatever results exist into BENCHMARKS.md.
bench-report:
    uv run --no-project benchmarks/aggregate.py

# Embedded engines end-to-end (Neo4j is opt-in: bench-neo4j-up → bench-neo4j).
bench-compare: bench-gen bench-drsg bench-sqlite bench-kuzu bench-report
    @echo "Embedded engines done. For Neo4j: just bench-neo4j-up && just bench-neo4j && just bench-report && just bench-neo4j-down"
