# Embedded CLI

`drsg` is the command-line interface. It opens the database **directly** — it is
embedded, not a client of a running server — which makes it suited to local
scripting, bulk ingestion, inspection, backup, and operations. Every invocation
takes a global `--db <path>` (default `graph.drsg`); most commands also take a
`--plane` (default `startup`).

Because it opens the database in-process, `drsg` cannot operate on a database
that a `drsg serve` — or another `drsg` — currently holds open: the database
takes an exclusive lock, and a second open fails with a clear error rather than
corrupting it. Use the SDKs or the RPC API against the server for concurrent
access, and the CLI for offline operations.

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
| `digest <file\|dir\|url> --plane [--mode]` | ingest a document, a repository, or a page and its links |
| `search <query> --plane` | semantic lookup: embeds the query, returns the nearest nodes |
| `context \| describe \| trace \| impact <name> --plane` | the agent verbs over a digested code plane |
| `vectorize --plane` | embed a plane's nodes for similarity search |
| `plugin install \| list \| remove` | manage preprocessor plugins (sandboxed wasm parsers) |
| `snapshot <out>` / `restore <in>` | whole-database backup and restore |
| `stats` / `check` | summary counts / integrity scan |
| `serve [--addr]` | run the web dashboard + JSON-RPC API + MCP endpoint |
| `serve watch [--dir]` | serve, and keep a code plane synced to every commit |

A digested file may be Markdown or plain text, or any of Word, PowerPoint,
Excel, OpenDocument, RTF, EPUB, CSV and PDF — those are converted to Markdown
first, so the model reads headings, tables and lists rather than loose
characters. The format is detected from the file's contents, so a wrong
extension still works. A directory is walked and routed per file: source code
goes through the installed parser plugins as facts, documents go to the model
as prose.

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

A node's `external_key` is its identity within a plane, so `--on-conflict`
decides what happens when an imported key is already there:

| Policy | Effect |
|---|---|
| `error` *(default)* | Import nothing and name the colliding keys |
| `skip` | Keep the existing node; drop the incoming one |
| `update` | Overwrite the existing node's properties, and its labels when the line carries them |

```console
$ drsg --db graph.drsg import nodes.jsonl --on-conflict update
imported 0 nodes, 0 edges, 2 existing updated
```

Under `skip` and `update`, edges in the file still resolve to the node already
in the plane, so relationships load against existing data. Edges themselves
carry no key, so they are always appended — the policy governs node identity,
not edge deduplication. Lines without an `external_key` cannot collide and are
always inserted.

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

$ drsg --db graph.drsg digest paper.md --plane papers --mode super --apply

$ drsg --db graph.drsg digest https://example.com/paper --plane papers \
    --topic "attention mechanism" --pages 6 --apply
```

An `http(s)://` argument is fetched rather than read from disk: the page is
converted to Markdown and its links followed under `--pages` / `--depth`, keeping
what is relevant to the page's own subject and to `--topic` if given ([Chapter
3](./ai-native.md#reading-from-a-url)). The scheme is required — a bare
`example.com` is a valid filename, and guessing which was meant would be worse
than asking. The command prints what it kept and what it dropped; there is no
selection prompt, which is what the dashboard's page list is for.

`--mode` selects how thoroughly the extraction is cleaned up: `coarse`
reconciles the label and edge-type vocabularies, `fine` (the default) also
merges entities that name the same thing, and `super` also re-reads every entity
against all of its passages — the most accurate, at roughly 15× the input token
usage ([Chapter 3](./ai-native.md#extraction-precision)).

## Backup and integrity

`snapshot` writes a consistent whole-database bundle at one commit sequence;
`restore` rebuilds it into an empty database, preserving ids, the commit
sequence, and the built search indexes ([Chapter 11](./architecture.md)).
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
