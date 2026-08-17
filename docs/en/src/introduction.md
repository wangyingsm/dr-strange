# Introduction

> 🌐 **English** · [中文版](../../zh/book/index.html)

**Dr Strange** is an AI-native embedded graph database, written in Rust.

It is a graph database designed from the outset for AI workloads: embeddings are
a first-class value type, similarity search operates alongside graph traversal,
and the engine exposes primitives suited to agents — natural-language queries, a
live change feed, and time-travel — rather than adding AI features to a
conventional graph database after the fact.

Like SQLite, it is **embedded**: a library linked into an application, backed by
a single on-disk database, with no server to operate. Unlike SQLite, it can also
**serve** — `drsg serve` exposes a JSON-RPC 2.0 API, a browser dashboard, and a
WebSocket change feed, with client SDKs in six languages.

## Intended audience

- **Application developers** constructing a knowledge graph, a GraphRAG
  pipeline, or an agent's long-term memory, requiring graph and vector storage
  in a single system.
- **AI and agent engineers** who expect the database to perform more of the
  work: compiling a question into a query, pushing changes as they commit, and
  reconstructing historical state on demand.
- **Readers** interested in the construction of an AI-native store.

## Organization

The early chapters establish the fundamentals: what Dr Strange is, how to build
it, and what its AI-native design entails. The middle chapters are a reference
for the query language and each access surface — the web UI, the language SDKs,
the command line, and the MCP server for LLM agents. The final chapter documents
the architecture.

> **Status.** This book is written alongside the **v1.0** release. Each chapter
> opens with a short introduction and a *draft outline* of its sections; the
> prose is filled in section by section.
