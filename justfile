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

# Build the container image (multi-stage: SPA + drsg binary + runtime).
docker-build tag="dr-strange:latest":
    docker build -t {{tag}} .

# Build and run the server via docker compose (persistent volume on :7700).
docker-up:
    docker compose up --build

# Build both editions of the tutorial book (mdBook) into docs/{en,zh}/book.
docs-build:
    cd docs/en && mdbook build
    cd docs/zh && mdbook build

# Live-serve one edition with hot reload (default English); e.g. `just docs-serve zh`.
docs-serve lang="en":
    cd docs/{{lang}} && mdbook serve --open

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
#
# Noise control: every engine is pinned to the same P-cores (on hybrid CPUs
# the P/E-core scheduling lottery is the largest run-to-run variance source)
# and runs `bench_repeat` measurement passes — the reported figures are the
# median across passes with the min→max spread recorded in the results JSON.
# Both knobs are overridable per machine: `just bench_pin="" bench_repeat=1 …`.

# Threads 0-15 are the 8 hyperthreaded P-cores on this machine's i9-14900HX;
# adjust the list (or empty it) for other machines.
bench_pin := "taskset -c 0-15"
bench_repeat := "3"

# Generate the shared dataset (deterministic).
bench-gen nodes="100000" edges="500000" dim="128":
    cargo build --release -p drsg-bench
    ./target/release/drsg-bench gen --out benchmarks/data --nodes {{nodes}} --edges {{edges}} --dim {{dim}}

bench-drsg:
    cargo build --release -p drsg-bench
    {{bench_pin}} ./target/release/drsg-bench run --data benchmarks/data --db benchmarks/data/drsg.db --out benchmarks/results/dr-strange.json --repeat {{bench_repeat}}

bench-sqlite:
    {{bench_pin}} uv run --no-project benchmarks/compare.py --engine sqlite --repeat {{bench_repeat}}

bench-kuzu:
    {{bench_pin}} uv run --no-project benchmarks/compare.py --engine kuzu --repeat {{bench_repeat}}

# Neo4j needs a running server; start/stop it around the run. The container is
# pinned to the same P-cores as the embedded engines, for fairness.
bench-neo4j-up:
    docker run -d --name drsg-neo4j --cpuset-cpus 0-15 -p 7687:7687 -p 7474:7474 \
      -e NEO4J_AUTH=neo4j/benchpass -e NEO4J_server_memory_heap_max__size=2G \
      -e NEO4J_server_memory_pagecache_size=1G neo4j:5.26
    @echo "waiting for neo4j..." && sleep 15

bench-neo4j-down:
    -docker rm -f drsg-neo4j

bench-neo4j:
    {{bench_pin}} uv run --no-project benchmarks/compare.py --engine neo4j --repeat {{bench_repeat}}

# Aggregate whatever results exist into BENCHMARKS.md.
bench-report:
    uv run --no-project benchmarks/aggregate.py

# dr-strange alone, no competitor engines: generate the shared dataset only if
# it is missing (bench-gen regenerates deterministically — delete
# benchmarks/data to force), run the drsg benchmark, refresh BENCHMARKS.md.
benchmark:
    @test -f benchmarks/data/meta.json || just bench-gen
    just bench-drsg
    just bench-report

# Embedded engines end-to-end (Neo4j is opt-in: bench-neo4j-up → bench-neo4j).
bench-compare: bench-gen bench-drsg bench-sqlite bench-kuzu bench-report
    @echo "Embedded engines done. For Neo4j: just bench-neo4j-up && just bench-neo4j && just bench-report && just bench-neo4j-down"

# The P0 eval board (revision plan): known resolution gaps as ignored tests,
# red until their phase lands. Failing here is the expected state — this
# recipe watches the reds turn green; it never gates CI. Pair with
# `just -f extensions/justfile eval` for the parser-side board.
eval:
    -cargo test -p dr-strange-llm --lib -- --ignored

# ---- the gate ------------------------------------------------------------

# Where `cargo build` actually drops the binary. CARGO_TARGET_DIR moves it, and
# the SDK suites treat a DRSG_BIN that points at nothing as *skip*, not fail —
# so the gate asserts this path rather than trusting it. Derived, not hardcoded,
# for the same reason PYTHONPATH is cleared below: the ambient environment is
# the thing that makes a local run and a CI run disagree.
drsg_bin := env_var_or_default("CARGO_TARGET_DIR", justfile_directory() / "target") / "debug" / "drsg"

# What `.github/workflows/ci.yml` runs, in its order, with its flags — so a
# local pass means a CI pass. `RUSTFLAGS: -D warnings` is the part that bites:
# without it clippy's findings are warnings locally and errors in CI, and a
# gate that greps the output rather than trusting the exit code hides them
# either way. Keep this recipe and that workflow in step: when one changes,
# change the other in the same commit.
#
# Everything CI runs, locally: run this before pushing.
gate: gate-rust gate-frontend gate-docs gate-sdk
    @echo "gate: every CI job passed locally"

# The redb pass is the one an all-defaults `cargo test` never covers: the
# storage backend is a cargo feature, and the other one has its own
# conformance suite.
#
# CI's `rust` job: fmt, clippy, test, and the redb backend.
gate-rust: _rust-matches-ci
    RUSTFLAGS="-D warnings" cargo fmt --all --check
    RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
    RUSTFLAGS="-D warnings" cargo test --workspace
    RUSTFLAGS="-D warnings" cargo test -p dr-strange-core --no-default-features --features redb-backend,json

# `bun install` unfrozen, as CI does — the committed lock is a bun-canary
# format a stable bun cannot parse frozen.
#
# CI's `frontend` job: lint, build, test the dashboard.
gate-frontend:
    cd crates/dr-strange-web/frontend && bun install && bun run lint && bun run build && bun test

# CI's `docs` job: both editions of the book must build.
gate-docs:
    mdbook build docs/en
    mdbook build docs/zh

# Each drives a real `drsg` through DRSG_BIN, exactly as CI hands them the one
# the rust job built.
#
# `PYTHONPATH` is cleared because CI has none and a developer machine often
# does — a ROS install on this one, whose `launch` package pytest then tries to
# import. Clearing it reproduces CI's environment rather than papering over a
# failure; any other ambient variable that makes the two disagree belongs here
# for the same reason.
#
# CI's five `sdk-*` jobs: drift + e2e against a real binary.
gate-sdk: _drsg-for-sdk
    @test -x "{{drsg_bin}}" || { echo "gate: no executable drsg at {{drsg_bin}} — every SDK suite skips a missing DRSG_BIN instead of failing, so the gate would pass with no e2e coverage. Point CARGO_TARGET_DIR at the real target dir, or unset CARGO_BUILD_TARGET." >&2; exit 1; }
    cd sdk/typescript && bun install && DRSG_BIN="{{drsg_bin}}" bun test
    cd sdk/python && DRSG_BIN="{{drsg_bin}}" env -u PYTHONPATH uv run --with pytest pytest -q
    cd sdk/go && DRSG_BIN="{{drsg_bin}}" go test ./...
    cd sdk/java && DRSG_BIN="{{drsg_bin}}" ./mvnw -q -B test
    cd sdk/c && DRSG_BIN="{{drsg_bin}}" make test

# RUSTFLAGS matches gate-rust because it is part of cargo's unit fingerprint:
# flip it and the whole graph recompiles. CI sets it job-wide, so its `Build
# drsg` step carries it too — dropping it here would both diverge from CI and
# build the workspace a second time on every gate run.
#
# The binary the SDK suites drive, as CI's `rust` job uploads it.
_drsg-for-sdk:
    RUSTFLAGS="-D warnings" cargo build -p dr-strange-cli

# CI installs the latest stable (`dtolnay/rust-toolchain@stable`), and an older
# stable has a strictly smaller clippy lint set — so a green gate here still
# goes red there the day a new stable lands a lint. Same drift the RUSTFLAGS
# note above describes, one level up: the flags agree, the compiler reading
# them does not. Needs the network, as CI does.
_rust-matches-ci:
    #!/usr/bin/env bash
    set -euo pipefail
    line=$(rustup check | grep '^stable-' || true)
    if [ -z "$line" ]; then
        echo "gate: 'rustup check' reported no stable toolchain — is rustup managing this one?" >&2
        exit 1
    fi
    case "$line" in
        *"update available"*)
            echo "gate: this stable is behind the one CI installs — run 'rustup update stable'" >&2
            echo "  $line" >&2
            exit 1
            ;;
    esac
    latest=${line#*up to date: }
    have=$(rustc --version | cut -d' ' -f2-)
    if [ "$latest" != "$have" ]; then
        echo "gate: cargo runs rustc $have, CI runs stable $latest — 'rustup default stable'" >&2
        exit 1
    fi
