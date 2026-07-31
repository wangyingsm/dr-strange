# MCP

`drsg-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io)
server that embeds Dr Strange, exposing the database to LLM agents as a set of
tools. An agent (in a compatible client) can search, query, traverse, run
algorithms, ask natural-language questions, and ingest documents — directly,
without you writing glue code.

## Why MCP

MCP is itself JSON-RPC 2.0, the same protocol the web backend speaks, so the MCP
server is a natural, first-class surface rather than an afterthought. It runs
over stdio and embeds the core engine, so an agent gets a real graph + vector
store as native tools.

## Sections (draft)

- What MCP is, and how Dr Strange fits an agent's toolset
- Running `drsg-mcp` and pointing a client at it
- The tools exposed (query, search, hybrid, algorithms, ask, digest, …)
- How tools map to the core API and the JSON-RPC surface
- Auth and safety (read vs. write, provider keys from the environment)
- Example: an agent building and querying a knowledge graph
