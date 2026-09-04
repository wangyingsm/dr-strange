# Getting Started

This chapter covers installing Dr Strange — from a released binary or from
source — initializing a database, issuing queries from the command line, and
running the server, both locally and as a container image.

## Prerequisites

Installing a released binary requires nothing beyond `curl` (or PowerShell on
Windows). The remaining prerequisites apply only to the other routes:

- Building from source: a current **Rust toolchain** (stable channel), installed
  via [rustup](https://rustup.rs).
- The web dashboard: **[bun](https://bun.sh)** to compile the single-page
  application, and optionally **[just](https://github.com/casey/just)** as the
  task runner.
- The container workflow: **Docker** (Engine 24+; BuildKit is only needed if you
  build the image yourself).

The build links TLS through rustls/ring; no OpenSSL toolchain is required.

## Installing a released binary

Every tagged release publishes binaries for Linux, macOS, and Windows. The
installer selects the archive matching the host platform, verifies its published
SHA-256, and places the binary on the `PATH`. Two binaries are available: the
command-line tool and server, `drsg`, and the MCP server for LLM agents,
`drsg-mcp` ([Chapter 8](./mcp.md)).

**Linux**

```console
# CLI and server — drsg
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh

# MCP server — drsg-mcp
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin drsg-mcp
```

**macOS** — the same script; both Apple silicon and Intel are published.

```console
# CLI and server — drsg
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh

# MCP server — drsg-mcp
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin drsg-mcp
```

**Windows**, in PowerShell. The second form runs the script as a block because a
script piped into `iex` cannot receive arguments.

```console
# CLI and server — drsg
PS> irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1 | iex

# MCP server — drsg-mcp
PS> & ([scriptblock]::Create((irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1))) -Bin drsg-mcp
```

Three options adjust the installation, each with an environment-variable
equivalent for non-interactive use:

| Option (Windows) | Environment variable | Effect |
|---|---|---|
| `--bin drsg-mcp` (`-Bin`) | `DRSG_INSTALL_BIN` | which binary — `drsg` (default), `drsg-mcp`, or `all` |
| `--version v1.1.0` (`-Version`) | `DRSG_VERSION` | pin a release instead of the latest |
| `--dir <path>` (`-Dir`) | `DRSG_INSTALL_DIR` | destination directory |

The destination defaults to `~/.local/bin`, and to
`%LOCALAPPDATA%\Programs\drsg\bin` on Windows, where the installer also adds the
directory to the user `PATH`. On Linux and macOS, add it to the shell profile if
it is not already present:

```console
$ export PATH="$HOME/.local/bin:$PATH"
```

### Upgrading

`drsg update` resolves the newest release the way the installer does — through
the redirect on `releases/latest`, rather than the API, which is rate-limited
for unauthenticated callers — and compares it with the running build. When
there is nothing to do it says so and stops:

```console
$ drsg update
drsg 2.4.2 is the latest release — nothing to do
```

When there is, it prints the command it is about to run and then *becomes* it:
the process is replaced by the same installer a first install runs, so the exit
status is the installer's own and there is no parent left waiting in a binary
that has just been overwritten.

```console
$ drsg update
drsg 2.4.1 -> 2.4.2
$ curl -fsSL .../install.sh | sh -s -- --bin drsg --dir '/home/me/.local/bin'
Dr Strange v2.4.2 (x86_64-unknown-linux-gnu)
  downloading dr-strange-v2.4.2-x86_64-unknown-linux-gnu.tar.gz
  checksum verified
  installed /home/me/.local/bin/drsg
```

The destination is the directory the running binary is in, not the installer's
default — an upgrade must replace the copy on the `PATH`, not add a newer one
somewhere else and leave the old one being run. `--dir` overrides it for a
`drsg` installed somewhere unwritable. A `drsg-mcp` in that same directory is
updated alongside `drsg` without being asked — the two binaries are one
release, and an agent host launching last release's server against this
release's `drsg` would have nothing to tell it so. `--bin` names exactly what
to update when that is not wanted: `drsg`, `drsg-mcp`, or `all`.

A build *newer* than the latest release — from source, or from a branch ahead
of the last tag — is told it is ahead and nothing is installed: `update` never
moves backwards. On Windows nothing is run at all, because the running
executable is locked against being overwritten; the command to paste into a
fresh terminal is printed instead.

The archives and their checksums may also be downloaded directly from the
[releases page](https://github.com/wangyingsm/dr-strange/releases); the
installers are a convenience over the same assets, and both scripts live in
[`scripts/`](https://github.com/wangyingsm/dr-strange/tree/master/scripts) for
inspection before use.

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
source_root = "/srv/myrepo"                 # source tree behind the grep/snippet agent tools (serve watch sets it from --dir)
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

[digest]                                    # server-side AIgest tuning
concurrency = 8                             # per-chunk extraction calls in flight
chunk_chars = 4000                          # target chunk size
embed_provider = "openai"                   # embedding provider for search / write_nodes / watch re-vectorization
embed_model = "text-embedding-3-small"      # its model (each provider has a default)
embed_key_env = "OPENAI_API_KEY"            # env var holding its key

[plugins]                                   # preprocessor sandbox tuning (all optional)
fuel = 200000000000                         # instruction budget per sandbox call (0 disables)
memory_mb = 3072                            # guest linear memory per call, MiB (wasm32 allows at most 4096)

[fetch]                                     # URL ingestion (Chapter 3)
enabled = true                              # false refuses URL fetching outright
max_pages = 10                              # ceiling on pages kept per crawl
max_depth = 3                               # ceiling on link-following depth a request may ask for
concurrency = 4                             # requests in flight
allow_private = []                          # see below — normally left empty
```

**`[fetch]` changes the server's network posture**, and is worth reading before
enabling anything in it. With URL ingestion on — the default — a client can name
an address that *the server* then connects to. The server's position on the
network is usually the more privileged one, so every non-routable address is
refused: loopback, RFC-1918 private space, link-local (`169.254.0.0/16`, where
cloud instance metadata services answer credentials), and the rest. The check is
made on the **resolved address**, not the hostname, and repeated at every
redirect hop.

`allow_private` re-permits specific CIDR blocks — `["10.0.0.0/8"]` to read an
intranet wiki — and is the one deliberate exception to that. It is not a switch
that turns the guard off, and a server reachable by untrusted clients should
leave it empty. To refuse URL fetching entirely, set `enabled = false`.

Precedence is fixed: an environment variable already set in the process always
takes precedence over the corresponding file value, and the `--addr` flag
overrides `[server].addr`. Providing `[server.tls]` switches the server to
HTTPS.

## Read-only replicas (`serve --follow`)

A second `drsg serve`, started with `--follow`, mirrors a running one
read-only — for scaling reads across a cluster of agents without funnelling
every query through one process:

```console
$ drsg --db replica.drsg serve --addr 127.0.0.1:7701 \
    --follow ws://master-host:7700 --follow-token please-change-me
```

Every write RPC is refused regardless of token. On startup — and again after
any disconnect — the replica pulls a full, consistent snapshot from the
master's `/snapshot` endpoint, then tails its `/ws/wal` for new commits;
every reconnect resyncs from scratch (arch/01 §9), so `--db` must name an
empty directory or one this same replica already owns (a `.drsg-follower`
marker records that) — anything else is refused rather than silently wiped.

`--follow-token` (or `DRSG_FOLLOW_TOKEN`) is the credential presented to the
master; it is independent of this replica's own `DRSG_TOKEN`, which still
gates its own downstream clients. Point `--follow` at `wss://` rather than
`ws://` for a master reachable over anything but loopback — the bearer token
otherwise travels in plaintext, the same requirement `[server.tls]` already
carries for inbound connections.

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
