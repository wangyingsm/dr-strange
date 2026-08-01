# syntax=docker/dockerfile:1
#
# Multi-stage build for `drsg serve`:
#   1. web     — compile the dashboard SPA (embedded into the binary)
#   2. build   — compile the `drsg` binary with the SPA baked in
#   3. runtime — a minimal image that serves the database
#
# The stack links TLS via rustls/ring, so no OpenSSL toolchain is required.

# ---- Stage 1: build the web dashboard (crates/dr-strange-web/frontend) ------
FROM oven/bun:1 AS web
WORKDIR /web
COPY crates/dr-strange-web/frontend/package.json crates/dr-strange-web/frontend/bun.lock ./
# Not --frozen-lockfile: the committed bun.lock is lockfileVersion 2 (bun canary),
# which this stable bun image can't parse; let it resolve fresh instead.
RUN bun install
COPY crates/dr-strange-web/frontend/ ./
RUN bun run build

# ---- Stage 2: build the `drsg` CLI (embeds frontend/dist at compile time) ---
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# The web crate's build.rs embeds this directory; supply the compiled SPA.
COPY --from=web /web/dist crates/dr-strange-web/frontend/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p dr-strange-cli && \
    install -Dm755 target/release/drsg /out/drsg

# ---- Stage 3: runtime -------------------------------------------------------
FROM debian:bookworm-slim AS runtime
# hadolint ignore=DL3008
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*
COPY --from=build /out/drsg /usr/local/bin/drsg

# The native-backend database is a directory; persist it on a volume.
VOLUME /data
EXPOSE 7700

# Bind to all interfaces inside the container (the default is loopback-only).
ENTRYPOINT ["drsg"]
CMD ["--db", "/data/graph.drsg", "serve", "--addr", "0.0.0.0:7700"]
