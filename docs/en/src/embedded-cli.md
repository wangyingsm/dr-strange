# Embedded CLI

`drsg` is the command-line tool for a local database. It opens the database file
directly (embedded — no server), which makes it ideal for scripting, ingest,
inspection, backup, and operations. Every command takes a global `--db <path>`.

## A tour

```console
$ drsg --db graph.drsg plane list
$ drsg --db graph.drsg import nodes.jsonl --plane social
$ drsg --db graph.drsg cypher --plane social 'MATCH (n:Person) RETURN n'
$ drsg --db graph.drsg algo pagerank --plane social --top 10
$ drsg --db graph.drsg ask 'who does Ada know?' --plane social
$ drsg --db graph.drsg snapshot backup.drsgsnap
$ drsg --db graph.drsg serve
```

## Backup and restore

`drsg snapshot <out>` writes a consistent, whole-database bundle at one commit
sequence; `drsg restore <in>` rebuilds it into a fresh (empty) database,
preserving ids, the commit sequence, and the built search indexes.

## Sections (draft)

- Global options (`--db`, config file) and output conventions
- Plane lifecycle (`plane create/list/show/drop`)
- Data in and out (`import` / `export` JSONL, `get`)
- Querying (`query` a plan, `cypher`, `catalog`)
- Graph algorithms (`algo pagerank/components/shortest-path/louvain`)
- Retrieval (`hybrid`, `index`, `ask`)
- Backup (`snapshot` / `restore`) and integrity (`check`, `stats`)
- Serving (`serve`) and how it relates to the SDKs
