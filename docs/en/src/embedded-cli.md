# Embedded CLI

`drsg` is the command-line interface. It opens the database **directly** — it is
embedded, not a client of a running server — which makes it suited to local
scripting, bulk ingestion, inspection, backup, and operations. Every invocation
takes a global `--db <path>` (default `graph.drsg`); most commands also take a
`--plane` (default `startup`).

Because it opens the database in-process, `drsg` should not operate on a database
that a `drsg serve` currently holds open; use the SDKs or the RPC API against the
server for concurrent access, and the CLI for offline operations.

## Command reference

| Command | Purpose |
|---|---|
| `init` | create an empty database (most commands also create on first use) |
| `plane list \| create \| drop \| show` | plane lifecycle |
| `import <file> --plane` | load JSONL nodes/edges |
| `export --plane` | dump a plane as JSONL |
| `get <node> --plane` | fetch one node by id or `@external-key` |
| `query <plan> --plane` | run a serialized logical plan (JSON, or `-`) |
| `cypher <query> --plane` | run an openCypher-subset statement (or `-`) |
| `catalog [--plane]` | print the soft schema (one plane or the whole database) |
| `algo <name> --plane` | run a graph algorithm |
| `hybrid <query> --plane` | fused vector + keyword + graph-proximity search |
| `index ensure \| keyword` | declare a vector or keyword index |
| `ask <question> --plane` | natural-language query |
| `digest <file> --plane` | ingest a document via an LLM |
| `snapshot <out>` / `restore <in>` | whole-database backup and restore |
| `stats` / `check` | summary counts / integrity scan |
| `serve [--addr]` | run the web dashboard + JSON-RPC API |

Global options are `--db <path>` and `--config <path>` (the configuration file,
[Chapter 2](./getting-started.md#configuration-file)).

## Planes

```console
$ drsg --db graph.drsg plane create social
$ drsg --db graph.drsg plane list
$ drsg --db graph.drsg plane show social
$ drsg --db graph.drsg plane drop social
```

## Data in and out

Import and export use JSONL — one node or edge per line — so a plane round-trips
through the file system and integrates with other tooling:

```console
$ drsg --db graph.drsg import nodes.jsonl --plane social
$ drsg --db graph.drsg export --plane social > social.jsonl
$ drsg --db graph.drsg get @ada --plane social
```

## Querying

A statement may be the openCypher subset or a serialized plan; either accepts `-`
to read from standard input. `--param` binds a `$name` placeholder to a JSON
value:

```console
$ drsg --db graph.drsg cypher --plane social \
    'MATCH (p:Person) WHERE p.age >= $min RETURN p' --param min=18

$ drsg --db graph.drsg query - --plane social < plan.json

$ drsg --db graph.drsg catalog --plane social
```

## Graph algorithms

```console
$ drsg --db graph.drsg algo pagerank      --plane social --top 10
$ drsg --db graph.drsg algo components    --plane social
$ drsg --db graph.drsg algo shortest-path --plane social --src 1 --dst 42
$ drsg --db graph.drsg algo louvain       --plane social
```

## Retrieval and ingestion

Declare indexes, run fused search, ask in natural language, and ingest documents
([Chapter 3](./ai-native.md)). The LLM-backed commands read provider keys from
the environment:

```console
$ drsg --db graph.drsg index ensure  Doc embedding --plane social
$ drsg --db graph.drsg index keyword Doc body      --plane social --lang english

$ drsg --db graph.drsg hybrid "how does time-travel work" \
    --plane social --label Doc --vector embedding --keyword body --graph-hops 1

$ drsg --db graph.drsg ask "which companies does Ada work for?" \
    --plane social --chat deepseek --embed qwen

$ drsg --db graph.drsg digest notes.md --plane social --apply
```

## Backup and integrity

`snapshot` writes a consistent whole-database bundle at one commit sequence;
`restore` rebuilds it into an empty database, preserving ids, the commit
sequence, and the built search indexes ([Chapter 9](./architecture.md)).
`stats` and `check` report counts and scan every plane for readability:

```console
$ drsg --db graph.drsg snapshot backup.drsgsnap
$ drsg --db fresh.drsg  restore  backup.drsgsnap
$ drsg --db graph.drsg stats
$ drsg --db graph.drsg check
```

## Serving

`serve` is the exception to the embedded model: it opens the database and then
exposes it over the network for the dashboard, the SDKs, and the MCP server.

```console
$ DRSG_TOKEN=please-change-me drsg --db graph.drsg serve --addr 0.0.0.0:7700
```

See [Chapter 2](./getting-started.md#running-the-server) for the server and its
configuration, [Chapter 5](./web-ui.md) for the dashboard, and [Chapter
6](./sdk.md) for the clients.
