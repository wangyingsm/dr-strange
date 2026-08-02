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
streams commits (colour-coded op badges, click-to-focus). SDKs: a long-lived
WebSocket `watch` in **all five** — TypeScript (native WS + auto-reconnect),
Python (blocking generator), Go (channel), Java (JDK `java.net.http.WebSocket`
+ listener), C (`drsg_watch` callback). TS uses the platform WebSocket and Java
the JDK's; Python/Go/C hand-roll a minimal RFC 6455 client (the installed
libcurl 7.81 predates its WS API). Each has an e2e test against a real server.

**Forks settled.** Best-effort in-memory broadcast (not a durable log); filter
plane + label now (predicate later); payload = full sanitized record inline;
slow consumer drops overflow (broadcast capacity 1024), never stalls writers.

---

## 6. Full-database backup / snapshot  *(shipped)*

**Status.** ✅ Shipped (2026-08-01). `Database::snapshot(w)` / `restore(r)` +
`drsg snapshot <out>` / `drsg restore <in>`. A consistent, whole-database
bundle at one pinned commit sequence: every plane's nodes/edges (with props),
index declarations, and the built vector/keyword sidecars. Restores into a
*fresh* (empty) database, preserving node/plane ids and the commit sequence via
a no-bump write path — so the shipped `.hnsw`/`.bm25` load as-is (no rebuild)
and future ids allocate past everything restored. Online (a pinned read
snapshot; take it quiesced for perfect sidecar fidelity). 3 core tests + a live
CLI round-trip.

**Forks settled.** Logical bundle (a length-prefixed postcard-frame stream —
restores to native/redb/memory) over a physical file copy; whole-DB into a
fresh target (refuses a non-empty one) over selective/merge; full only (no
incremental); ship the built sidecars (id fidelity keeps them valid) over
rebuild-on-restore. **Follow-ups:** incremental (since-seq), selective
plane-level restore/merge, and a web "download snapshot" action.

---

## 7. Query-language parity — Cypher subset ⇒ full engine  *(shipped)*

**Status.** ✅ Shipped (2026-08-02). The openCypher subset now reaches
everything the engine does, so the language is a complete alternative to
hand-writing `LogicalPlan` JSON — and a first-class LLM target:

- **Key-seek** — `key(n)` is an ordinary expression term (`Expr::ExternalKey`);
  an equality or `IN` on the *source* variable compiles to a `SeekKeys` seek
  rather than a scan-and-filter, so a query can anchor on an entity whose
  identity lives in the key — the common shape for LLM-digested graphs.
- **Keyword / BM25 search** — `SEARCH (d:Doc) ON body MATCHING "…" [TOPK k]`.
  Same verb as the vector seed, different operator: `NEAR` compares meaning,
  `MATCHING` compares words.
- **Hybrid search** — `HYBRID (d:Doc) [VECTOR …] [KEYWORD …] [GRAPH …]
  [CANDIDATES n] [TOPK k]`, each channel optional with its own `WEIGHT`.
- **Graph algorithms** — `CALL <pagerank|components|shortest_path|louvain>(args)
  ON (n[:Label])`, where `ON` both scopes the algorithm and binds the variable.
- **Time-travel** — a trailing `AS OF <seq|"RFC-3339"|TIME ms>` clause.
- **Typed expansion from a seed** — every source may carry a relationship tail,
  so a normal typed hop follows a retrieval or algorithm seed.
- Plus `x IN [a, b]` over any expression, desugared to equalities.

**Why AI-native.** Cypher is a surface LLMs write fluently and *tersely* — a far
better NL→query target than the bespoke, verbose `LogicalPlan` JSON, which is
prone to blowing the model's output-token budget on broad questions. The Cypher
route (item 3) now matches the plan route's *capability*, and humans/SDKs reach
the full engine through a single language.

**Forks settled.** The plan IR grew (`Source::KeywordTopK` / `Hybrid` / `Algo`,
executed through `GraphReader`, which gained `keyword_search`) rather than the
parser routing around it — so `plane.query` and the SDKs gain the same power,
and every new source composes with the existing steps. `key(n)` in `WHERE` over
a dedicated `SEEK` clause; `CALL name(args) ON (v:Label)` over a bespoke clause;
per-channel `WEIGHT` on `HYBRID`; `AS OF` last, so every existing query keeps
its exact prefix. Algorithm results ride the score channel — rank for PageRank,
path position for shortest path, a dense community index for components/Louvain
(aggregation & projection stay deferred, see Low priority). One hybrid engine
now backs both `plane.hybrid()` and `Source::Hybrid`. `AS OF` is not a plan
node — it addresses the plane handle — so it rides on `ReadQuery` beside the
plan for a surface to apply. The keyword / hybrid / algorithm / temporal forms
are read-only.

**Follow-ups.** Expose the new sources in the dashboard's query surface.

---

## 8. Extraction precision — AIgest in three passes

**Goal.** Make the graph AIgest produces *precise*: one node per real entity,
one label per real kind, one edge type per real relationship, and properties
drawn from everything the document says about an entity rather than from
whichever chunk happened to mention it first.

**The problem, measured.** Extraction is one round: each chunk goes to the model
alone and the results are merged positionally. `merge_props` is explicit about
the consequence — *"never clobbering an existing key (first chunk wins)"* — so an
entity's properties, its `description`, and even its **label** are fixed by its
earliest mention, and every later, better mention is discarded. Relations dedup
on `(src, dst, ty)`, first wins likewise. No chunk ever sees what another chunk
extracted, so nothing converges on a shared vocabulary. Digesting the
"Attention Is All You Need" paper into the `attention` plane gives, for 108
nodes and 113 edges:

- **43 distinct labels**, including `Attention Mechanism` vs `AttentionMechanism`;
- **48 distinct edge types**, including `COMPARED_TO`, `COMPARED_WITH`,
  `COMPARED_IN` and `CONTRASTS_WITH` side by side — only the last holds the
  answer to "what is the Transformer compared with", which makes the graph
  hostile to both humans and NL→query;
- **10 key pairs differing only in case or spacing** — `Multi-Head Attention` /
  `Multi-head attention`, `Self-Attention` / `Self-attention`, `Softmax` /
  `softmax`, `d_model` / `dmodel`, `K` / `k`, `Q` / `q`;
- **split identities**: `K` and `Key` are two nodes, as are `Q`/`Query`,
  `V`/`Value`, and `Transformer` / `Transformer (base model)` /
  `Transformer (big)`;
- **two external keys held by two nodes each** (`ByteNet`, `ConvS2S`) — a key
  lookup silently returns one of them.

That last one is a correctness bug, not a quality complaint: duplicate
prevention runs *entirely* through vector linking (`PlaneCandidates` → the
`existing` set), so a plane whose nodes carry no usable embedding — as this one
does, every `embedding` being an empty vector — links nothing and re-creates
nodes under keys the plane already holds. There is no exact-key fallback.

**Why AI-native.** Ingestion quality is the ceiling on everything above it. A
fragmented vocabulary defeats NL→query (the model picks a plausible edge type
that holds no data), defeats hybrid retrieval (the same entity's signal is split
across duplicate nodes), and defeats traversal (a question anchored on
`Transformer` misses what is attached to `Transformer (base model)`). This is
the highest-leverage remaining work: it improves every read path at once,
without changing any of them.

**Scope sketch — three passes, delivered in order, each shippable alone.**

*Stage 1 — vocabulary reconciliation (cheapest, largest win).* After round 1,
send the model the **label set** and the **edge-type set** alone — a few hundred
tokens, one or two calls regardless of document size — and have it return a
canonical mapping (`AttentionMechanism → Attention Mechanism`,
`COMPARED_TO|COMPARED_WITH|CONTRASTS_WITH → COMPARED_WITH`). Apply it to the
extraction before merge. Cost is O(1) in the document; the win is most of the
fragmentation above.

*Stage 2 — identity resolution.* Merge nodes that denote the same entity
(`K`/`Key`, case variants) and decide the containment cases deliberately — is
`Transformer (big)` its own node, or a property of `Transformer`? Includes the
bug fix: an **exact-key check against the plane** before creating a node, so
duplicate prevention no longer depends on embeddings being present and usable.
Candidate pairs come from cheap signals first (normalized key equality, prefix
containment) with the model adjudicating only the genuinely ambiguous ones.

*Stage 3 — per-entity refinement.* For each surviving entity, gather **every**
occurrence in the document plus its relations, and re-ask the model for that
entity's properties and description with the full picture in front of it. This
is the round that repairs first-chunk-wins. Run concurrently like round 1, merge
in a deterministic order. Occurrences come from the chunks that produced the
entity (recorded during round 1 — not tracked today, and cheap to add) widened
by BM25 over the chunk text, reusing the in-tree `text::Analyzer` so it needs no
embedder.

**Settled — stage 3 cost.** Gate on *possibility*, rank on *value*; they are
different questions. Refinement can only add information when an entity has
occurrences **outside the chunk(s) that produced it**, which the occurrence pass
computes for free, model-free — so entities with nothing new to read are skipped
without a call and without loss. Survivors are ranked by degree plus property
sparsity, because the measured graph shows importance and thinness coincide: the
hubs are the thinnest (`Scaled Dot-Product Attention`, degree 11, one property;
`Multi-Head Attention`, degree 6, one property; 61% of nodes are degree ≤1 and
58% carry only a description). Degree alone is rejected as the *gate* — it
measures importance, not whether anything remains to be learned.

Two budgets, not one, since the larger cost is input rather than calls: a hub
mentioned throughout a document would otherwise carry nearly the whole text as
context. Cap **entities refined** and **occurrences per entity** (top-*m* by
BM25, always including the producing chunks). Both default to **unlimited** —
correctness first, with the cost visible in `DigestReport` — and both are
`DigestOptions` knobs beside `concurrency` / `chunk_chars`.

Batching several entities per call is rejected: it would cut call count but
multiply prompt size and let entities contaminate each other's refinement.
**Per-entity isolation is what makes the pass trustworthy.**

The pass must report refined / skipped / why plus the mention-spread histogram,
so the thresholds are tuned from real runs rather than guessed — that
distribution is unknown today because chunk provenance is not recorded.

**Settled — reconciliation keeps the original wording.** A canonicalized label
or edge type carries the canonical form, and the form the document actually used
is recorded beside it as an underscore-prefixed provenance property
(`_label_as_written` / `_type_as_written`), written only where the two differ.
Provenance properties are already hidden from the schema summary the model
reads, so aliases cost the read paths nothing while keeping the document's own
words recoverable. Stage 2 inherits this mechanism unchanged: an entity merged
into another carries the same alias record forward rather than inventing a
second scheme.

**Forks to settle.**
- *Edges are visited twice* in stage 3, once from each endpoint, yielding two
  refinements of one edge. Resolve by endpoint order, by confidence, or refine
  edges in their own pass?
- *Drift* — "here is every mention, refine" invites invention. Constrain the
  round to the supplied contexts, and require evidence (a quote or offset) for
  a changed value, versus trusting the model's judgement?
- *Where reconciliation applies* — rewrite the extraction before it is merged,
  or write both and record the canonical form as an alias property (keeping the
  document's own wording recoverable)?
- *Scope of stage 2* — within one digest run only, or also against entities
  already in the target plane (which is where the duplicate-key bug bites)?
- *Determinism* — stage 1's mapping and stage 3's merge must be reproducible
  for the same input, as round 1's chunk-order merge already is.

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
