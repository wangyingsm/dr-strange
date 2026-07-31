# Introduction

> 🌐 **English** · [中文版](../../zh/book/index.html)

**Dr Strange** is an AI-native embedded graph database, written in Rust.

It is a graph database designed from day one for AI workloads — embeddings are a
first-class value type, similarity search sits next to graph traversal, and the
engine speaks the language of agents (natural-language queries, a live change
feed, time-travel) rather than a classic graph database with AI features bolted
on afterward.

Like SQLite, it is **embedded**: a library you link into your program, with a
single on-disk database and no server to operate. Unlike SQLite, it can also
**serve** — `drsg serve` exposes a JSON-RPC 2.0 API, a browser dashboard, and a
WebSocket change feed, with client SDKs in five languages.

## Who this book is for

- **Application developers** building a knowledge graph, a GraphRAG pipeline, or
  an agent's long-term memory, who want graph + vector in one place.
- **AI/agent engineers** who want the database to do more of the work: turn a
  question into a query, push changes as they happen, and answer "what did this
  look like last Tuesday".
- **The curious**, who want to see how an AI-native store is put together.

## How this book is organized

The early chapters get you productive — what Dr Strange is, how to install it,
and what makes it AI-native. The middle chapters are a reference for the query
language and each way of talking to the database: the web UI, the language SDKs,
the command line, and the MCP server for LLM agents. The final chapter opens the
hood on the architecture.

> **Status.** This book is written alongside the **v1.0** release. Each chapter
> opens with a short introduction and a *draft outline* of its sections; the
> prose is filled in section by section.
