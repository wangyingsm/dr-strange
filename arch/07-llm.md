# LLM Layer

**Status**: draft for review · 2026-07-22 · **digest pipeline design deferred**

Scope: the `dr-strange-llm` crate — everything that talks to a language or embedding
model. Sits strictly **above** the core: `dr-strange-core` never calls a model, never
sees an API key, and remains fully usable with this crate absent. `dr-strange-cli`
and `dr-strange-mcp` depend on it optionally.

## 1. Responsibilities

| Capability | Description |
|---|---|
| Embedding generation | text → vector at ingest/query time; pluggable providers (Anthropic-compatible / OpenAI-compatible / local HTTP), batching, retry; per-plane model configuration recorded as plane properties, so mixed-model vectors are detectable |
| Document digestion | the engine behind `drsg digest` / MCP `digest`: LLM parses documents into entities, relations, `PropDesc` descriptions, and embeddings, written through the bulk API — **detailed design deferred; to be discussed separately** |
| Entity resolution | propose cross-plane / intra-plane duplicate candidates by external key, name similarity, and embedding distance; output is a *proposal set* the caller (human or agent) confirms — feeds plane `merge` (09 §3) |
| NL → plan translation | natural-language question → serialized logical plan, grounded on the per-plane catalog (labels + property descriptions); v1.5, once the plan format is stable |

## 2. Design rules

- **Proposals, not mutations**: model outputs (extractions, match candidates,
  generated plans) are values returned to the caller; writing them is a
  separate, inspectable step (`--dry-run` is the default posture; MCP callers
  confirm). Keeps hallucination damage bounded and auditable.
- **Provenance on everything written**: digested nodes/edges carry properties
  recording source document, model, and run id — using `PropDesc`
  descriptions so provenance is itself self-explaining. One digest run = one
  plane by default.
- Provider abstraction is minimal: `trait Embedder` and `trait Chat` with
  plain HTTP implementations (JSON-RPC where the provider supports it, REST
  otherwise); no agent-framework dependency.
- Cost controls: token/request budgets per digest run, surfaced in progress
  output; embedding cache keyed by content hash to avoid re-embedding
  unchanged text.

## 3. Open questions

1. **Digest pipeline** (chunking, extraction schemas/prompting, incremental
   re-digest, dedup-during-ingest, cost model) — deferred to its own design
   session; this doc holds only its boundary: documents + API key in, plane
   of nodes/edges/vectors + report out.
2. **Local model support** — llama.cpp/ollama-compatible endpoints for
   embeddings: needed in v1?
3. **NL → plan safety** — generated plans are read-only by construction in
   v1.5? (Leaning yes: the NL interface never mutates.)
