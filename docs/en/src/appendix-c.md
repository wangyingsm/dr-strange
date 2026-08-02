# Appendix C: LLM Included or Not

Dr Strange calls an external language model for a specific, bounded set of
features. Everything else — the graph store, vector and keyword indexes, hybrid
retrieval over pre-computed vectors, graph algorithms, time-travel, the change
feed, and backup — runs with no model at all.

This appendix delineates exactly which features require model support, how to run
Dr Strange with none, and how to point the model features at a local model
instead of a hosted API. In all cases, provider API keys are read from the
server's environment (or the `[llm]` configuration section), never from a request
or tool parameter.

## What features need LLM support

A model is invoked in two situations: to **embed text** (turn a string into a
vector), and to **chat** (generate structure or a plan). The features that depend
on one or both:

| Feature | Needs | Why |
|---|---|---|
| Embedding a **text** similarity query (`SEARCH … NEAR "text"`, semantic `plane.find`, a hybrid vector channel from text) | embedding provider | the query string is embedded server-side before the search |
| **Natural-language query** (`ask` / `plane.ask`) | chat provider (+ embedding provider for the grounding tools) | the model compiles the question into a plan, optionally calling embedding-backed `find_edge` / `find_entity` tools |
| **Document ingestion** (`digest` / AIgest) | chat + embedding providers | the model extracts entities and relations, then cleans up the extraction (`--mode`); the entities are embedded |

Everything else requires no model:

- **Storing and searching vectors you already have.** A vector is an ordinary
  property; declare an index and search it with a **literal** vector
  (`SEARCH … NEAR $vec`). Only *text* queries need embedding.
- **Keyword search.** BM25 is purely lexical.
- **Graph queries and traversal.** `MATCH`, `SEARCH … NEAR $vec`, `plane.query`,
  `plane.neighbors`, `graph.seed` / `graph.expand`.
- **Graph algorithms.** PageRank, components, shortest path, Louvain.
- **Time-travel, the change feed, and backup/restore.**
- **The hybrid keyword and graph channels**, and the vector channel when given a
  literal vector.

In short: the model is needed only to turn *text* into vectors, and to drive
`ask` and `digest`. If you embed your data with your own pipeline and query by
literal vector, keyword, and graph, Dr Strange needs no model.

## Running Dr Strange without LLM

There are two independent ways to run without a model: simply not using the model
features, and building a binary with the model code excluded entirely.

### Not using the model features

The model-backed operations call a provider only when invoked. Supply no provider
keys and avoid `ask`, `digest`, and text-embedding queries, and the rest of the
system is fully functional. Embed your data with your own pipeline, store the
vectors as properties, and query by:

- **literal-vector similarity** — `SEARCH (d:Doc) ON embedding NEAR $vec TOPK 10`,
- **keyword** — a BM25 index (`index keyword …`),
- **graph** — `MATCH`, traversal, and the graph algorithms.

A served instance still exposes `ask` and `digest`, but they return a clear error
when no provider key is configured; nothing else is affected.

### Building without the model code

The command-line tool gates the model features behind the `digest` Cargo feature,
which pulls in the LLM crate. Build without it for a lean binary that has no model
dependency at all:

```console
$ cargo build --release -p dr-strange-cli --no-default-features --features native-backend
```

The resulting `drsg` omits the `ask` and `digest` commands; every other command
— planes, import/export, queries, indexes, hybrid (keyword and literal-vector
channels), algorithms, snapshot/restore, and `serve` — is unchanged.

## Use local LLM / models

The model features do not require a hosted API. A provider is either a **preset**
name or a **base URL**, so any OpenAI-compatible endpoint — including a local one
— can serve chat and embeddings.

### Ollama

[Ollama](https://ollama.com) exposes an OpenAI-compatible API locally. The
built-in `ollama` preset points at `http://localhost:11434/v1`, needs no key, and
defaults to `llama3.1` for chat and `nomic-embed-text` for embeddings:

```console
$ ollama pull llama3.1
$ ollama pull nomic-embed-text

$ drsg --db graph.drsg ask "which companies does Ada work for?" \
    --plane social --chat ollama --embed ollama

$ drsg --db graph.drsg digest notes.md --plane social --apply \
    --chat ollama --embed ollama
```

Override the models with `--model` and `--embed-model` as needed.

### Any OpenAI-compatible server

A local inference server that speaks the OpenAI API — vLLM, LM Studio,
llama.cpp's server, and others — is addressed by passing its **base URL** as the
provider, with the model named explicitly:

```console
$ drsg --db graph.drsg ask "…" --plane social \
    --chat  http://localhost:8000/v1 --model       my-chat-model \
    --embed http://localhost:8000/v1 --embed-model my-embed-model
```

If the endpoint requires a key, set it in the environment and name its variable
with the key-environment options; a keyless local server needs none.

### In the dashboard, server, and MCP

The same providers apply everywhere the model is used. The **AIgest** page and the
semantic search selector list the presets, including `ollama`; the server and the
MCP host read the provider keys (or none, for a keyless local server) from their
environment. Chat and embedding providers are chosen independently, so a local
chat model may be paired with a local — or hosted — embedding model, and vice
versa.
