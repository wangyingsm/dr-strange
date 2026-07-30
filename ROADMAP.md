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

## 1. Graph algorithms  *(first)*

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

## 2. Hybrid retrieval + fusion  *(second)*

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

## 3. Natural-language querying (NL→plan)  *(third)*

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

## 4. Time-travel / temporal queries  *(fourth)*

**Goal.** Query the graph as of a past point: "what did this look like at
sequence/time T".

**Why AI-native.** Agents need historical state for auditing, replay, and
"what changed". The native engine's **MVCC sequence numbers make historical
snapshots nearly free** — the machinery exists, only the surface is missing.

**Scope sketch.** Expose a read snapshot pinned to a chosen commit sequence
(or a wall-clock → seq mapping), plumbed through `PlaneHandle` reads and a
query option (`AS OF <seq|time>`). Requires the native backend's retention to
keep the needed versions (interacts with compaction GC — a pinned historical
snapshot must be honored like a live reader, or bounded by a retention window).

**Forks to settle.** Backend support (native MVCC only, or emulate on redb?);
seq vs timestamp addressing (needs a seq→time index if timestamps);
retention policy (unbounded history vs a TTL/window bounding compaction GC).

---

## 5. Change subscriptions / CDC  *(fifth — was #7)*

**Goal.** "Watch" a query, label, or plane and receive a stream of changes as
they commit — reactive agents subscribing to graph mutations.

**Why AI-native.** Agents that react to the graph (trigger on new entities,
maintain derived state) need push, not poll. The WebSocket already pushes
stats; this generalizes it to change events.

**Scope sketch.** A commit-time change feed (the write path already buffers
coherence events + bumps the commit sequence) delivered over WS as a
subscription; filter by plane / label / (later) a predicate. At-least-once with
a resume-from-seq cursor.

**Forks to settle.** Delivery semantics (best-effort vs durable log); filter
granularity (plane/label now, predicate later); payload (full record vs id +
change kind); backpressure / slow-consumer handling.

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
