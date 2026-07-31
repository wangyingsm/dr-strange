# dr-strange — AI-native feature roadmap

Forward-looking plan for the AI-native capabilities that most differentiate
dr-strange as a graph database. Ordered by priority. Each item lists the goal,
why it matters for an AI-native graph DB, a scope sketch, and the design forks
to settle before building.

Foundations already in place (context): storage backends (memory / redb /
native LSM, MVCC + durable), graph model (planes, external keys, bulk load),
an openCypher-subset query language over a serializable `LogicalPlan` + `Expr`,
HNSW vector search with hybrid operators (`VectorTopK` / `FrontierTopK` /
`ExpandBeam`), LLM ingest (`digest`) with entity linking + embeddings, and the
surfaces: `drsg` CLI, MCP server, web dashboard, JSON-RPC API, 6 SDKs.

---

## 1. Graph algorithms  *(shipped)*

**Status.** ✅ Shipped (2026-07-31). `compute::algo` + `plane.algo()` with
PageRank, weakly-connected components, weighted shortest path (Dijkstra), and
Louvain community detection — whole-plane or label-scoped, read-only/transient
results. Exposed on every surface: RPC `plane.algo`, CLI `drsg algo …`, MCP
`algo` tool, and the web dashboard (an "Analyze" bar in the Explore view that
overlays PageRank size / community + component colour / shortest-path highlight
onto the graph plot). (Follow-ups: subgraph/seeded scoping and optional
property-materialization of scores; SDK regeneration from the new OpenRPC entry.)

**Goal.** A library of classic graph algorithms exposed as first-class
operations: **PageRank / centrality** (node importance), **shortest path**
(weighted Dijkstra / A\*), **community detection** (Louvain — clustering for
summarization), and **connected components**.

**Why AI-native.** These are the backbone of GraphRAG and knowledge-graph
reasoning: "what's important near X", "how are A and B connected", "summarize
this cluster". Today only `expand` / `FrontierTopK` / `ExpandBeam` exist — no
importance, no paths, no clustering.

**Scope sketch.** New compute operators + a `plane.algo` surface (RPC + CLI +
MCP). Read over a single snapshot (`GraphReader`), returning node→score or
node→component / a path. Deterministic where possible.

**Forks to settle.** Run in-executor vs a separate algo module; whole-plane vs
subgraph-scoped; exact vs approximate (e.g. PageRank iterations, Louvain
resolution); result shape (scores as a transient result vs materialized as
properties).

---

## 2. Hybrid retrieval + fusion  *(shipped)*

**Status.** ✅ Shipped (2026-07-31). BM25 keyword index (`text::Analyzer` with
Snowball stemming + per-index language; `keyword::KeywordRegistry` mirroring the
HNSW registry — declared, coherent on writes, `.bm25` sidecar) + a
`plane.hybrid()` fusion of **three** channels (vector + BM25 + graph proximity)
via weighted, min-max-normalized score fusion, each hit reporting its
per-channel breakdown. Surfaced on RPC `plane.hybrid`, CLI `drsg hybrid` +
`drsg index keyword`, MCP `hybrid`, and the web dashboard (a "Hybrid" bar in the
Explore view that ranks the plane, plots hits sized by fused score, and lists
them with per-channel breakdown; and index declaration is now self-service —
`plane.indexes` lists what's declared and `index.ensure` declares from the UI,
so the Hybrid bar only offers real channels and can create missing ones).
(Follow-ups: RRF as an alternative fusion; `index.ensure` on MCP too — a gap
shared with the vector index.)

**Goal.** True hybrid retrieval that combines **vector** + **keyword/full-text**
+ **graph proximity** into one ranked result, via a fusion operator
(Reciprocal Rank Fusion or weighted score fusion).

**Why AI-native.** Hybrid retrieval is table stakes for RAG. Vector search is
strong, but keyword search is a linear scan (`plane.find`, capped at ~20k
nodes, no index) and there is **no fusion** combining the signals. This is the
most direct RAG win and it's mostly one index + one operator away.

**Scope sketch.** (a) A real **inverted index / BM25** over string properties
(a new index type alongside the HNSW registry), incrementally maintained like
the vector index. (b) A **fusion** operator/query form that merges ranked lists
(vector top-k, BM25 top-k, optional graph-expansion boost) into a single
scored result. Wire into the query language (`SEARCH … HYBRID`?), RPC, CLI, MCP.

**Forks to settle.** Inverted-index storage (new KV tables vs a lib); tokenizer
/ analyzer (stemming, stopwords, language); fusion method (RRF vs weighted) and
default weights; whether graph proximity is a third channel or a re-rank pass.

---

## 3. Natural-language querying (NL→plan)  *(shipped)*

**Status.** ✅ Shipped (2026-07-31). `dr_strange_llm::ask` grounds the model
with the plane catalog + a compact LogicalPlan-JSON spec, emits a plan,
deserializes it (read-only by construction — no write operators), runs it, and
repairs on parse/exec error (bounded attempts); a safety `Limit` is appended
and `dry_run` returns the validated plan without running. Surfaced on RPC
`plane.ask`, CLI `drsg ask`, MCP `ask`, and the web dashboard (an "Ask" tab in
the Explore view that runs the plan, plots the results, and shows the generated
plan JSON) — each returns the generated plan (for transparency) + result rows.
Chat key stays server-side. **Agentic grounding:** given an embedder, `ask` is
a ReAct tool loop — the model calls `find_edge` (embed a relationship phrase →
rank real edge types, cross-lingual: 任职→EMPLOYED_AT) and `find_entity` (embed
a name → vector-search node embeddings for the real key) before planning, so it
stops guessing edge types / entity keys. (Follow-ups: NL→vector-search
*operators* in the plan itself.)

**Goal.** Ask a question in English and get a graph answer: an LLM translates
NL into a `LogicalPlan` (or openCypher-subset text) that the engine runs.

**Why AI-native.** The marquee affordance. The DB already *ingests* with an LLM
(`digest`); letting it be *queried* in natural language closes the loop. The
`LogicalPlan` / `Expr` are already serializable, so an LLM can target them
directly with the soft-schema catalog as grounding.

**Scope sketch.** A `plane.ask` (RPC + CLI + MCP): embed the schema catalog +
question → LLM emits a plan/Cypher → validate → execute → return result (+ the
generated plan for transparency). Reuse the `dr-strange-llm` provider layer;
keys stay server-side.

**Forks to settle.** Emit openCypher text vs `LogicalPlan` JSON directly;
validation/repair loop on a bad plan; read-only by default (gate writes);
how much schema/context to feed; return the plan for user confirmation before
executing (agent-safe) vs auto-run.

---

## 4. Time-travel / temporal queries  *(shipped)*

**Status.** ✅ Shipped (2026-08-01). Native backend only — the LSM engine keeps
prior versions keyed by commit sequence, so historical snapshots are near-free.
`native-backend` is now the default engine for the whole workspace (redb is the
opt-in legacy path). `NativeEngine::begin_read_at(seq)` pins a past snapshot
(registered like a live reader so compaction honours it) with a retention
window (`Database::set_retention`, default unbounded) that floors compaction GC
so history stays reachable. `AsOf::{Seq,Time}` + `PlaneHandle::as_of(..)` return
a read-only handle whose queries, traversals, algorithms, hybrid search, and
point lookups all observe the historical snapshot; the address resolves by
binary search over the retained window (commit seq / wall-clock time are both
monotonic in the snapshot — no per-commit index). `Database::history()` reports
the queryable window. Web RPC: `plane.history` + `as_of`/`as_of_ms` on
`plane.query`/`plane.neighbors`. The whole surface is compile-time gated to
`native-backend` in the core (the wire contract stays uniform and errors
clearly on redb). **Remaining surface:** CLI (`drsg`), MCP tool, web dashboard
time slider.

**Forks settled.** Native MVCC only (redb/memory reject AS OF); both seq and
timestamp addressing (commit-time stamped in Meta on every write); retention
default unbounded with an opt-in commit-count window bounding compaction GC.

---

## 5. Change subscriptions / CDC  *(shipped)*

**Status.** ✅ Shipped (2026-08-01). Commit-time change feed, end to end.
`Database::on_change(observer)` fires a `ChangeSet { plane, seq, changes }`
after every committed write; `WriteTxn` buffers `(kind, id, op)` per mutation
(free unless an observer is registered), collapsed per entity at commit
(create-then-delete cancels), capped, with each created/updated record read
back at the committed snapshot. Payload is the full record with embeddings and
`_`-prefixed internal props stripped; a delete carries id only (pair with
`as_of(seq-1)`). Web: the observer feeds a broadcast channel; `/ws` gains
`plane.watch { plane, label? }` / `plane.unwatch` per-connection subscriptions,
delivering `plane.change` notifications. Dashboard: a "Live" Explore tab
streams commits (colour-coded op badges, click-to-focus). SDKs: `watch()` over
a long-lived WebSocket in TypeScript (native WS, auto-reconnect) and Python (a
hand-rolled zero-dep RFC 6455 client, blocking generator). **Remaining:**
WebSocket `watch` for the go/c/java SDKs (their generated surfaces are synced;
the WS client is hand-written per language — Java has stdlib `java.net.http`,
go/c need a small frame client).

**Forks settled.** Best-effort in-memory broadcast (not a durable log); filter
plane + label now (predicate later); payload = full sanitized record inline;
slow consumer drops overflow (broadcast capacity 1024), never stalls writers.

---

## 6. Full-database backup / snapshot  *(sixth — was #8)*

**Goal.** An atomic, consistent whole-database snapshot + restore (and
point-in-time recovery), beyond the current per-plane JSONL export.

**Why AI-native.** Operational safety for a knowledge graph you're building
continuously. The MVCC sequence again makes a **consistent snapshot cheap**
(pin a seq, copy).

**Scope sketch.** `drsg snapshot <out>` / `restore <in>`: a consistent dump
across all planes + the vector-index sidecar at one commit sequence; native
backend can snapshot by pinning a seq + copying SSTs/WAL. Streamed format for
large DBs.

**Forks to settle.** Format (logical JSONL bundle vs physical file copy);
online (no-lock, via a pinned snapshot) vs offline; include/rebuild the HNSW
sidecar; incremental backups (since-seq) vs full only.

---

## 7. Conversational AI chat → plan → run  *(seventh — capstone)*

**Goal.** A multi-turn **chat** surface over the graph: the user converses in
natural language, the assistant builds (and iteratively refines) a
`LogicalPlan`, runs it, and grounds its answers in the results — carrying
conversation context across turns ("now filter those to last week", "why?").

**Why AI-native.** This is the capstone that ties the whole DB together. Item 3
(NL→plan) is the one-shot primitive; this is the *agentic* loop around it —
memory of prior turns, follow-up refinement, tool-use over the query engine
(`plane.ask` for retrieval, `plane.algo` for reasoning, hybrid retrieval for
grounding), and self-correction when a generated plan fails. It turns
dr-strange from a queryable graph into a graph you can *talk to*.

**Scope sketch.** A `plane.chat` surface (RPC + CLI REPL + MCP + web chat panel):
a conversation state (history + the schema catalog + last result set as
context) drives an LLM tool-use loop whose tools are the existing query
primitives (NL→plan, algorithms, hybrid search, raw plan execution). Each turn:
plan → validate → execute → summarize, with a repair loop on failure. Reuses
the `dr-strange-llm` provider layer; keys stay server-side. Built **last**
because it depends on items 1–3 (algorithms + hybrid retrieval + NL→plan) as
its toolset.

**Forks to settle.** Where conversation state lives (client-held vs
server-session vs persisted in a plane); tool-use protocol (native LLM
tool-calling vs a hand-rolled ReAct loop); read-only vs write-capable chat
(let it mutate the graph?); streaming responses; how much result data to feed
back as grounding vs summarize; per-conversation cost/turn limits.

---

## Low priority (deferred — not first-class for now)

These are real graph-DB table stakes but explicitly **not** a current priority.

- **Aggregation & projections in the query language** (`count/sum/avg/collect`,
  `GROUP BY`, `WITH`-pipelining, `RETURN a.name, count(*)`). Foundational for
  analytics but the most invasive change — it needs the multi-binding row model
  (a path/row result contract instead of the current single-current-node
  model), which ripples to every surface. Deferred deliberately.

- **Constraints / schema validation** (uniqueness, required properties,
  edge-cardinality enforcement). The soft-schema catalog *observes* structure
  but doesn't *enforce* it. Nice for integrity; not a differentiator.
