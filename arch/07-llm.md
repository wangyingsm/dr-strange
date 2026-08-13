# LLM Layer

**Status**: digest pipeline designed and shipped (ROADMAP §8, extended to URLs
in §9 and to preprocessor plugins in §11) · last revised 2026-08-14

Scope: the `dr-strange-llm` crate — everything that talks to a language or embedding
model, **plus the ingestion front door that feeds it**. Sits strictly **above** the
core: `dr-strange-core` never calls a model, never sees an API key, and remains fully
usable with this crate absent. `dr-strange-cli` and `dr-strange-mcp` depend on it
optionally.

Document → Markdown conversion lives here despite talking to no model, because it
is the digest pipeline's own input step and because every surface that ingests a
document — the CLI, the MCP tools, the dashboard — must produce *identical* text
from the same file. Two readers would mean two vector spaces for one corpus.

## 1. Responsibilities

| Capability | Description |
|---|---|
| Embedding generation | text → vector at ingest/query time; pluggable providers (OpenAI-compatible, which covers gateways and a local `ollama`/`llama.cpp` through a configurable base URL), batched per call. Configured **per server** (`[digest] embed_provider`), not per plane: a per-plane model recorded as plane properties was considered and not built, since nothing yet needs to detect mixed-model vectors and the config an operator actually sets is process-wide |
| Document reading | bytes → GitHub-Flavored Markdown for Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV and PDF (via `anydoc`), with Markdown and plain text passing through. Format is detected from the content, not the filename. Deterministic and model-free — the step before digestion, shared by every surface |
| Preprocessing | an input's own structure → **facts** (nodes and edges a parser is certain of) plus **prose** (the residue needing a model), routed per file so a polyglot tree fans out and merges (ROADMAP §11). Handlers are **installed wasm plugins** (`drsg plugin install`, SHA-256 pinned, sandboxed: no filesystem, no network, frozen clocks, fuel- and memory-bounded; contract and SDKs live in the `dr-strange-extensions` repo). Document reading is the built-in fallback every unclaimed input lands on. An input yielding only facts is digested with **no model call at all**. Local-only: the CLI and the stdio MCP server, never a shared server — see §2 |
| Document digestion | the engine behind `drsg digest` / MCP `digest`: an LLM parses that Markdown into entities, relations, `PropDesc` descriptions, and embeddings, written through the bulk API. Shipped as AIgest's three passes (ROADMAP §8) |
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
  plane by default. A preprocessor's facts carry `_generated_by` (`rust@1`)
  instead of `_model`, so a parsed fact is always distinguishable from a
  model's guess; where both claim one key, **the fact wins** and the model's is
  dropped and counted.
- **Preprocessing is local-only**: what makes parsing worth its cost is a
  plugin pulling the files *around* the one it was handed — and that pull is
  exactly what a shared server must not offer, since the only filesystem it
  could reach is the server's own. So the CLI and the stdio MCP server route
  through it; `drsg serve` and the HTTP MCP server do not, and text sent over
  the wire stays prose. *What the host will answer* is the capability grant,
  rather than a policy document beside it that can drift.
- Provider abstraction is minimal: `trait Embedder` and `trait Chat` with
  plain HTTP implementations (JSON-RPC where the provider supports it, REST
  otherwise); no agent-framework dependency.
- Cost controls: token/request budgets per digest run, surfaced in progress
  output; embedding cache keyed by content hash to avoid re-embedding
  unchanged text.

## 3. Decisions

All three questions this doc opened with are settled.

1. **Digest pipeline** — designed and shipped as AIgest's three passes
   (ROADMAP §8), later extended to read URLs (§9). This doc still holds only
   the boundary: documents + API key in, a plane of nodes/edges/vectors +
   report out.
2. **Local model support** — settled: yes, and it needed no dedicated code
   path. `openai.rs` takes a configurable `base_url`, so any
   OpenAI-compatible endpoint works — a gateway, `ollama`, or `llama.cpp`.
3. **NL → plan safety** — settled: yes, and it is *enforced*, not merely
   intended. `dr-strange-parser`'s `read_only()` rejects any statement that
   would mutate before it can become a `ReadQuery`, so the NL interface
   cannot write even if the model emits a mutation.
