# Getting Started

This chapter covers building Dr Strange from source, initializing a database,
issuing queries from the command line, and running the server — both locally and
as a container image.

## Prerequisites

- A current **Rust toolchain** (stable channel), installed via
  [rustup](https://rustup.rs).
- For the web dashboard: **[bun](https://bun.sh)** to compile the single-page
  application, and optionally **[just](https://github.com/casey/just)** as the
  task runner.
- For the container workflow: **Docker** (Engine 24+; BuildKit is only needed if
  you build the image yourself).

The build links TLS through rustls/ring; no OpenSSL toolchain is required.

## Building from source

Dr Strange is a Cargo workspace. Compile the command-line binary, `drsg`:

```console
$ cargo build --release -p dr-strange-cli
```

The artifact is `target/release/drsg`. The default build selects the native LSM
storage engine; the legacy redb backend remains available behind a feature flag.

The web dashboard is embedded into the binary at compile time by the web crate's
build script. To embed the compiled dashboard rather than a placeholder page,
build the single-page application before the binary:

```console
$ just web-build          # bun install && vite build
$ cargo build --release -p dr-strange-cli
```

The **MCP server** for LLM agents ([Chapter 8](./mcp.md)) is a separate binary,
`drsg-mcp`:

```console
$ cargo build --release -p dr-strange-mcp
```

The artifact is `target/release/drsg-mcp`. Place it on the `PATH`, or reference
it by absolute path in the host configuration.

## On-disk layout

The `--db` argument selects the database path. Under the native backend the
database is a **directory** — the write-ahead log and the sorted SST files reside
within it — accompanied by two sidecar files that hold the search indexes:

```text
graph.drsg/          database (WAL + SST files)
graph.drsg.hnsw      vector-index sidecar
graph.drsg.bm25      keyword-index sidecar
```

The database is created on first access; there is no separate initialization
step.

## Creating a graph

Create a plane, then insert data. The openCypher subset compiles to the same
logical plan the engine executes directly:

```console
$ drsg --db graph.drsg plane create social

$ drsg --db graph.drsg cypher --plane social \
    'CREATE (a:Person {name:"Ada"}),
            (b:Person {name:"Alan"}),
            (a)-[:KNOWS]->(b)'
```

Query the result:

```console
$ drsg --db graph.drsg cypher --plane social \
    'MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN q'
```

Inspect the resulting shape and soft schema:

```console
$ drsg --db graph.drsg stats
$ drsg --db graph.drsg catalog --plane social
```

## Vector index and similarity search

Vectors are ordinary property values. Declare an index on a `(label, property)`
pair, then query it. Embedding a text query is performed server-side and
therefore requires a provider key in the process environment (for example,
`OPENAI_API_KEY`); a query against a literal vector requires no provider.

```console
$ drsg --db graph.drsg index ensure Doc embedding --plane social

$ OPENAI_API_KEY=… drsg --db graph.drsg cypher --plane social \
    'SEARCH (d:Doc) ON embedding NEAR "a friendly greeting" TOPK 5 RETURN d'
```

Because the results are graph nodes, traversal can continue from them — the
GraphRAG pattern introduced in Chapter 1.

## Running the server

```console
$ drsg --db graph.drsg serve
```

This starts the JSON-RPC 2.0 API, the WebSocket change feed, and the embedded
dashboard, and reports the bound address (default `127.0.0.1:7700`).

**Authentication.** With no token configured, only the same-origin browser UI is
authorized to call the API. To permit programmatic access from the SDKs or
`curl`, configure a shared token and present it as a bearer credential:

```console
$ DRSG_TOKEN=please-change-me drsg --db graph.drsg serve
```

### Configuration file

Server, logging, and provider settings may be supplied through a TOML
configuration file instead of individual flags and environment variables. The
file is resolved from `--config <path>`, then `$DRSG_CONFIG`, then `./drsg.toml`
if present. Unknown keys are rejected.

```toml
[server]
addr = "0.0.0.0:7700"                       # bind address (CLI --addr overrides)
token = "please-change-me"                  # shared API token (→ DRSG_TOKEN)
max_concurrent = 256                        # ceiling on in-flight requests
allowed_origins = ["https://app.example.com"]  # additional browser origins

[server.tls]                                # present ⇒ serve HTTPS
cert = "/etc/drsg/cert.pem"                 # PEM certificate chain
key  = "/etc/drsg/key.pem"                  # PEM private key

[logging]
dir = "/var/log/drsg"                       # directory for the rolling log file

[llm]                                       # provider keys, exported to the environment
OPENAI_API_KEY = "sk-…"
DEEPSEEK_API_KEY = "…"
DASHSCOPE_API_KEY = "…"
```

Precedence is fixed: an environment variable already set in the process always
takes precedence over the corresponding file value, and the `--addr` flag
overrides `[server].addr`. Providing `[server.tls]` switches the server to
HTTPS.

## Container image

A multi-arch image (`linux/amd64` and `linux/arm64`) is published to the GitHub
Container Registry. Pull and run it — no build required:

```console
$ docker run -p 7700:7700 -v drsg-data:/data \
    -e DRSG_TOKEN=please-change-me \
    ghcr.io/wangyingsm/dr-strange:latest
```

`docker run` pulls the image on first use. Pin a release with a version tag —
`ghcr.io/wangyingsm/dr-strange:1.0.2` — instead of `:latest` for reproducible
deployments. The runtime image binds to `0.0.0.0:7700` and stores the database on
the `/data` volume (the native backend database is a directory, which the volume
persists). Provider keys are supplied as environment variables.

For a persistent deployment, `docker-compose.yml` pulls the same image and defines
a named volume:

```console
$ DRSG_TOKEN=please-change-me docker compose up
```

To build the image locally instead, the repository ships a multi-stage
`Dockerfile` that compiles the dashboard, embeds it in the binary, and produces a
minimal runtime image: `docker build -t dr-strange:latest .`.

## Next steps

- **Chapter 3 — AI Native:** embeddings, hybrid retrieval, natural-language
  querying, and document ingestion.
- **Chapter 4 — Query Language:** the openCypher subset and the underlying
  logical plan.
- By interface: **Chapter 6 — SDK** (application code), **Chapter 7 — Embedded
  CLI** (operations), **Chapter 8 — MCP** (LLM agents).
