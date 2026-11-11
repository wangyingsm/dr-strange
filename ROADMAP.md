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

**Added later: `toposort`** (issue #98), for a plane whose edges mean *feeds*.
Whole-plane or label-scoped like the rest, plus an `edge_type` naming the one
relation that claims the order and a `dir` for planes whose edges point
dependent→dependency. Each node carries a `generation` — its longest-path
depth, so one generation can run in parallel. A cyclic scope answers with a
node *on* the cycle instead of the partial order Kahn's algorithm leaves
behind. On `plane.algo`, the CLI and the MCP tool; not a Cypher `CALL`, since
`CALL` reports one scalar per node through `score()` and an order gives two.

**Added later: `cycles`** (issue #99), the other half of that question — a
write that must not create a loop has to refuse *naming the nodes at fault*.
Each result is the node list closing one cycle (`[12, 7, 9, 12]`); an empty
result is the "is this a DAG" answer, which `components` cannot give, since a
DAG and a cyclic graph are both one weakly connected component. `limit` caps
the search rather than the output, so `limit: 1` stops at the first cycle. One
depth-first pass reporting one cycle per back edge: enough to prove a graph is
cyclic and to name the fault, not an enumeration of every cycle, which is
exponential. No `dir` — reversing every edge maps a cycle onto its reverse.

**Open: `toposort` in the dashboard** (issue #98). The algorithm and the RPC
arm exist; the Explore view's "Analyze" bar does not offer it. It needs more
than another entry in that menu, which is why it is not done yet:

- An **edge-type input**, since an order is a statement about one relation.
  The existing overlays read every edge, so this is the bar's first control
  that changes which edges an algorithm sees at all.
- **Colour by `generation`**, not by category. The component/community overlay
  assigns an arbitrary colour per group; generation is ordered, so it wants a
  sequential ramp, with the legend reading as a scale rather than a key — the
  point being to see at a glance which nodes can run together.
- **A cyclic plane answers with an error**, not an empty overlay. The node it
  names is on the cycle, so the view should select and centre it: that node is
  the whole reason the question failed.

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
default unbounded in the library, with a commit-count window bounding
compaction GC — `drsg serve` sets that window to 20 commits (`[server]
retain_commits`, 0 for unbounded), because a watched repository rewrites
nodes on every commit and unbounded history made the store, and every
compaction over it, grow for as long as the server lived.

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
(aggregation & projection landed later, in §13). One hybrid engine
now backs both `plane.hybrid()` and `Source::Hybrid`. `AS OF` is not a plan
node — it addresses the plane handle — so it rides on `ReadQuery` beside the
plan for a surface to apply. The keyword / hybrid / algorithm / temporal forms
are read-only.

**Follow-ups.** Expose the new sources in the dashboard's query surface.

---

## 8. Extraction precision — AIgest in three passes  *(shipped)*

**Status.** ✅ Shipped (2026-08-03). All three passes are built, live-run
against the paper this section measured, and exposed as a single choice —
`DigestMode::{Coarse, Fine, Super}` — on every surface (`--mode`, `digest.run`'s
`mode`, the MCP tool, and a select in the dashboard's AIgest view). Each mode
adds a pass to the one before it, so the ordering is the cost ordering and a
caller picks a point on it rather than assembling a combination of knobs.

**Measured, one `super` run over the same document:**

| | before | after |
|---|---|---|
| labels | 70 | **39** (5 folded, 61 merged) |
| edge types | 67 | **32** |
| entities | 248 | **230** (3 folded, 15 merged, 27 pairs adjudicated) |
| properties | — | **368 added, 109 revised** across 104 refined entities |

104 of 230 entities had something to read outside the chunks that produced
them; the other 126 were skipped without a call, as the gate intends. No
refinement failed. Cost is the headline: **21.6k input tokens at `fine`,
320k at `super`** — ~15×, since every eligible entity carries its passages into
a call of its own. That figure is stated in the CLI help, the OpenRPC method
summary (which is what reaches SDK users), and an amber notice in the dashboard.

The duplicate-key correctness bug is fixed: `CandidateSource` gains an exact-key
lookup, so duplicate prevention no longer depends on embeddings being present
and usable.

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

**Forks settled.**
- *Edges visited twice* — dissolved rather than decided. Stage 3 rewrites node
  properties only; an entity's relations go **into** the prompt as context and
  never come back out as writes, so no edge is refined twice because none is
  refined at all. Edge-property refinement is a follow-up, not a fork.
- *Drift* — constrained to the supplied contexts, without demanding a quote per
  value. The prompt is one entity and its passages; a value not in them has
  nothing to come from. Two hard rules do the rest: provenance properties
  (`_`-prefixed) and `embedding` are never writable by refinement, and a
  refinement that fails costs that entity rather than the run.
- *Where reconciliation applies* — both. The extraction is rewritten before the
  merge (so the merge itself converges), and the form the document used is kept
  beside it as `_label_as_written` / `_type_as_written` / `_key_as_written`,
  written only where the two differ.
- *Scope of stage 2* — split by cost of being wrong. **Exact** key matching runs
  against the plane, which is what closes the duplicate-key bug; **fuzzy**
  matching stays within the run. Merging an extracted entity into an existing
  node on a model's word is a larger question, deliberately left open.
- *Determinism* — stage 1 asks for *groups* of names rather than a from→into
  map, and a name claimed by an earlier group is not re-claimed by a later one,
  so the result no longer depends on rename order. Stage 3's concurrent workers
  tally locally and merge in candidate order. Both are reproducible for the same
  model output. (The un-grouped phrasing was measurably unstable: the same
  document, model and prompt merged 78 names on one run and 15 on the next.)

**Follow-ups.**
- Stage 2 only *proposes* pairs by containment, so two genuinely different names
  for one thing (`self-attention` / `intra-attention`) remain invisible without
  a similarity signal.
- Fuzzy matching against entities already in the target plane (see above).
- Refining edge properties, which stage 3 currently reads but never writes.

---

## 9. URL ingestion — AIgest reads the web  *(shipped)*

**Status.** ✅ Shipped (2026-08-03). A URL is a third input beside upload and
paste, on all three surfaces: `drsg digest <url>`, a streaming `/digest/fetch`
endpoint, and a URL row in the dashboard's AIgest view that returns a *list* of
what was found — each page with its relevance score, ticked if it cleared the
floor — so nothing becomes tokens before the reader has seen it.

Live against the Wikipedia article on Transformers: 1062 candidate links, 5
pages read, the rest reported in one line rather than a thousand. The crawl
found two failure modes no unit test would have:

- **A site's own machinery outranked its content.** `/w/index.php?title=
  Transformer_(deep_learning)&action=edit` scored above the article on attention,
  because the query string repeated every word the target was looking for. URL
  *paths* are now read and queries are not: a query parameterizes a view of a
  document rather than naming one.
- **`robots.txt` matching was too loose.** Wikipedia publishes a
  `User-agent: Fetch` group (an offline-download tool) with `Disallow: /`, and a
  substring test made `drsg-fetch` obey it and refuse the entire site. Group
  selection now matches the longest token that is a **prefix of our product
  token**, as the convention intends.

**Goal.** A URL becomes a third input to AIgest beside upload and paste. The
server fetches the page, converts it to Markdown, follows its hyperlinks as far
as a budget allows, keeps only what is relevant to the target, and assembles one
LLM-ready document that the existing digest pipeline consumes unchanged.

**Why AI-native.** Most material worth digesting lives behind a URL rather than
on a disk, and a page's outbound links are a *curated bibliography* — the author
already decided what is related, which is a signal no amount of retrieval
recreates. It also plays directly into §8: a crawl is the case where one entity
is mentioned across several documents, which is exactly what first-chunk-wins
handles worst and what `super` mode repairs.

**The problem.** Following links naively is how a knowledge graph fills with
cookie banners, "Privacy Policy", and navigation chrome. Relevance has to be
decided, and **hop count does not decide it** — depth measures how far the crawl
walked, not whether the page is about anything. A footer link is one hop away
and pure noise; a linked appendix three hops out can be the substance.

**Scope sketch.**

- A **server-side fetcher**. A browser `fetch()` is blocked by CORS for
  essentially every third-party site, so this cannot live in the dashboard. It
  is also a new production HTTP dependency: `reqwest` is currently a
  *dev*-dependency of `dr-strange-web`, used only by the tests, with no TLS
  backend enabled.
- **HTML → Markdown** with main-content extraction, not the tag-stripping
  `strip_tags` in `extract.rs` (which exists for DOCX XML and would drag nav and
  footer text in as prose). Markdown because the chunker already handles it and
  because headings and lists survive the conversion.
- **Two-gate relevance** (below), model-free, over `text::Analyzer` — the same
  tokenizer, stemmer and stopword set the BM25 index uses, so the whole system
  keeps one notion of what a word is.
- **Budgets**: max pages, max depth, max total bytes, per-host concurrency and
  rate, total wall-clock. Whatever a budget drops is reported rather than
  silently truncated.
- **Assembly** into one document, each page preceded by its URL and title, with
  a page boundary forcing a chunk boundary so no chunk straddles two pages.
- **Surfaces**: a streaming endpoint following the `/digest/extract` precedent
  (a crawl needs progress for the same reason PDF extraction does), a selection
  list in the dashboard, and a URL in place of a path on the CLI.

**Settled — relevance is decided twice, and hops are only a tiebreak.** The two
gates answer different questions, in the shape §8 established (*gate on
possibility, rank on value*):

*Before fetching*, score each candidate link on what is already in hand for
free — its anchor text, `title`, and the words in its URL path — by BM25 against
the target terms. No network, no model. Only the top candidates are fetched.

*After fetching*, re-score the extracted text and **drop** pages that do not hold
up. A link promising "Transformer architecture" that delivers a login page dies
here, having cost exactly one request.

Hop decay (`score × decay^hops`) rides along as a tiebreak toward the root, the
same idiom `plane.hybrid`'s graph channel already uses. It does not decide
relevance. Depth defaults to 1; the budget is the real control, since depth 2 is
50 links × 50 links.

**Settled — the target comes from both.** The relevance target defaults to the
root page's own top terms, and an optional topic the user types sharpens it —
much the better signal when the user knows what they are after. Offering only
the root page makes a broad landing page unusable as a seed; offering only a
typed topic makes the common case (paste a URL, press go) require homework.

**Settled — one blob into the text input.** Fetched pages are concatenated and
land in the same textarea that upload and paste fill, so they stay editable and
nothing downstream changes: `digest.run` gains no parameter, and the mode /
preview / write flow is untouched. Page URLs travel as header lines rather than
as structured provenance. Carrying pages structurally so each node records the
page it came from is better provenance and real new plumbing through the digest
API — a second stage, not this one.

**Settled — fetching is on by default; reaching the private network is not.**
The feature ships enabled, because a database that has to be reconfigured before
it can read a URL will mostly not be used to read URLs. The security posture is
carried by guards that are *not* part of that default:

- `http`/`https` only;
- DNS resolved and the **resolved address** checked against loopback, private,
  link-local (`169.254.0.0/16` — cloud metadata) and multicast ranges, at every
  redirect hop rather than once, since checking the hostname alone leaves DNS
  rebinding open;
- response size, redirect count and total time capped;
- `robots.txt` respected, an identifying User-Agent sent, and per-host rate
  limiting applied — the bandwidth being spent belongs to someone else.

Reaching a private address is therefore not a setting to relax casually but an
explicit allowlist for an operator who means it. The posture change — a database
server that makes outbound connections to addresses its clients choose — is
stated where an operator will see it: the configuration reference, the book's
server chapter, and the CLI help.

**Settled — the CLI takes a URL too.** `drsg digest <url>` costs little once the
fetcher exists. It has nowhere to show a selection list, so it selects by
threshold and reports what it kept and dropped; the interactive list is a
dashboard affordance, not a requirement of the feature.

**Forks settled.**
- *How the cut is made* — both, because they bound different things. The page
  budget is a rank cap; the floor is **relative to the best page in the batch**
  (a quarter of it, by default), so it adapts to a corpus instead of asserting
  an absolute meaning for a BM25 score.
- *Cross-origin* — follow anywhere and let relevance decide, since citations are
  exactly what lives off-origin. Placement is a nudge rather than a rule: a link
  in the prose scores 1.25×, one on the same host 1.15×. A documentation table
  of contents lives in a `<nav>` and must still be able to win.
- *Linked documents* — a linked PDF goes through the existing `extract.rs` path,
  under the same per-response size cap that bounds everything else. Free reach,
  bounded appetite.
- *Re-fetching* — dissolved. A fetch happens once, on an explicit press, and its
  result lands in the text box; changing the mode and previewing again re-reads
  the box, not the network. No cache is needed because nothing re-crawls.
- *JavaScript-rendered pages* — a documented limitation. A headless renderer is
  a browser, and shipping one inside a database is not a small decision.
- *Topic and root terms* — merged, with a typed term weighted 3× against the
  page's own most frequent terms. It sharpens rather than replaces.

**Follow-ups.**
- Relevance is scored in one language per crawl (the analyzer's), so a
  multilingual site is read through one stemmer.
- Coverage is not length-normalized, so a very long URL — an archive.org wrapper
  embedding another URL — can out-match a short one on the pre-fetch gate.
- The dashboard offers no per-crawl budget controls; the CLI has `--pages` /
  `--depth` and the server caps both.

---

## 10. MCP over the network — one database, many agents  *(shipped)*

**Goal.** Let several agent hosts share one memory. Today each host spawns its
own `drsg-mcp`, which embeds the core and opens the database directly, so two
editors on one project are two writers on one file. As of v1.4.2 the second is
refused with a clear error instead of silently destroying the first one's
writes — that is a floor, not an answer.

**Why AI-native.** This is the use case the README leads with. A memory layer
only one agent can hold open is a memory layer for one agent, and the
interesting behaviour — one agent reading what another wrote — is exactly what
the current deployment model forbids.

**The problem.** `drsg-mcp` embeds by design, the same way `drsg` does: point a
host at a path and it works, with nothing to run and nothing to configure.
`drsg serve` is the networked surface, for the SDKs, the dashboard and the
JSON-RPC API — one process, one `Database`, `write_gate` serializing writers and
MVCC giving genuinely concurrent readers. Both models are complete; neither
serves several agents sharing one memory, because the protocol agent hosts
actually speak reaches only the embedded one.

**Settled — the transport belongs on `serve`, not a proxy in `drsg-mcp`.** The
cheaper-looking option is a `drsg-mcp --connect <url>` forwarding each tool call
to a running server over `/rpc`. It is rejected on the reporter's own evidence
in issue #1: the tools do not map cleanly onto the RPC surface. `traverse` has
no method and would have to be rebuilt as a `LogicalPlan`; `write_nodes` /
`write_edges` become N round-trips and **lose batch atomicity**; `digest` has to
be composed from `digest.run` + `digest.write`. Proxying would quietly change
what the tools mean, and an agent's memory is the last place to accept "almost
the same semantics".

An MCP transport hosted by `drsg serve` avoids all of it — the tools run in the
same process against the same `Database`, with the batch atomicity and the
traversal code they already have. More work up front, less work forever.

**Scope sketch.** An HTTP/SSE MCP endpoint on `drsg serve` (rmcp is already a
dependency; only `transport-io` is compiled in today). The 15 tools move behind
a transport-independent handler, so the stdio binary and the served endpoint
drive the same code. `drsg-mcp` keeps its embedded stdio mode — it is the right
answer for a single agent and needs no infrastructure.

**Forks to settle.**
- *Authentication.* `auth.rs`'s `Access::{Read, Write, Admin}` is the natural
  base for per-agent scoping, but the v1 model is one shared token that
  authorizes everything. Scoped keys, or one token and per-agent planes?
- *The token posture is a trap for newcomers.* With no `DRSG_TOKEN` set only the
  same-origin browser UI is trusted, and every programmatic client is denied
  **even for reads** — deliberate, so a zero-config desktop install does not
  quietly expose an open API on localhost. Any remote mode must state this where
  someone configuring it will read it.
- *Isolation.* Do agents share a plane, or does each get its own beside a shared
  one? Sharing is the point, but two agents writing the same entity concurrently
  is a merge problem this project already has opinions about (§8).
- *Transport.* Streamable HTTP versus SSE, and what each costs in host support.
- *Whether the embedded binary gains a client mode at all* once the served
  endpoint exists — a host can point straight at the URL.

**Credit.** Raised by @maidol in issue #1, alongside the multi-process bug fixed
in v1.4.2.

---

## 11. Preprocessor plugins — domain structure before the model

**Goal.** Every source of truth carries structure of its own, and that structure
is knowledge the model should not have to rediscover from prose. A plugin turns
a format-specific input into **facts** (nodes and edges it is certain about) and
**prose** (the residue that needs understanding), and a router dispatches to the
right one before the digest pipeline runs. Source code is the first and best
case; the point is that stock series, building models and everything else stay
*out* of this repository and arrive as plugins.

**Why AI-native.** Three wins, and only the first is the obvious one.

*Tokens.* An interface-level view of a source file — signatures, types, doc
comments, call edges — is a small fraction of the file, and none of the body
text ever reaches the model.

*Precision.* An AST does not infer that `parse()` calls `lex()`; it **knows**.
Handing that to a model as prose so it can re-derive a `CALLS` edge spends
tokens to get a worse answer than the one already in hand.

*Vocabulary.* Plugin facts carry a schema fixed by the plugin — `Function`,
`Trait`, `IMPLEMENTS` are constants, not inventions — so §8's fragmentation
problem does not arise for that portion of the graph and stage 1 has nothing to
reconcile there. A facts-only plugin is a digest with **no model call at all**,
which puts a whole class of ingestion on the LLM-free side of Appendix C.

**The problem.** `document::to_markdown(name, bytes) -> String` is already this
router — it just has the table compiled in (anydoc by signature, then extension;
`.txt`/`.md` passthrough) and only ever produces text. This item generalizes the
table and widens the return type. It is also the seam §9 lands on: a fetched URL
arrives as bytes plus a content type, which is the same routing question a
file's extension asks, and there should be one dispatch point rather than two.

**Scope sketch.**

- A **transport-independent protocol**: `describe` → manifest (name, version,
  what it handles, what it wants access to), optional `detect(sample)` →
  confidence, and `preprocess` → `{ facts, prose, report }`. `facts` uses
  exactly the node/edge shape `digest.write` already accepts, so plugin output
  is writable through a path that exists.
- Input by **pull, not push**: the plugin calls back into the host to list and
  read the files it needs. A whole repository pushed into wasm32 linear memory
  is both a needless copy and a 4 GiB ceiling; pulling also lets a code plugin
  follow imports across files, which is where the call graph lives — and *what
  the host will answer* is the capability grant, rather than a policy document
  beside it.
- A **router** over built-in handlers and plugins, resolving on declared
  extensions and content types first, with an explicit user override always
  winning. `detect` is for genuine ambiguity only: a router that guesses is
  worse than one that asks.
- **Grounding.** The facts are handed to the LLM pass as known entity keys, so
  the model attaches to them instead of inventing parallel nodes — which §8
  stage 2's exact-key check already handles.
- **Two example plugins**, Rust and Go, in-tree as the proof that the interface
  is real and as the template a third party copies.

**Settled — facts and prose, not prose alone.** A plugin returns both. This is
the whole reason the item is worth building: preprocessing that only condenses
text saves tokens, while preprocessing that emits facts also removes an entire
class of extraction error.

**Settled — the built-in extractors stay built in.** PDF, DOCX and plain text
remain compiled into the server rather than being rewritten as plugins, so a
default install keeps working with nothing configured and no wasm runtime
present. They sit behind the same router, so the interface is uniform even where
the implementation is not.

**Settled — interface level only.** A code plugin emits signatures, types,
trait/interface implementations, doc comments and call edges. Statement-level
ASTs would produce a graph no human can read and no model can afford, for
material that belongs in a compiler rather than a knowledge graph.

**Settled — provenance is a property.** Every node and edge a plugin produces
carries `_generated_by: <plugin-name>`, so a later reader can always separate a
parsed fact from a model's guess. Underscore-prefixed properties are already
hidden from the schema summary the model reads, so this costs the read paths
nothing — the mechanism §8 established for `_label_as_written`.

**Settled — WASM, behind a cargo feature that is on by default.** Plugins are
WebAssembly modules run in-process by an embedded runtime, gated behind a
`plugins` feature — compile-time gated rather than runtime-erroring, per the
existing convention — and that feature ships **on**, following `digest`'s
precedent in the CLI: the capability is there for anyone who wants it, and
`--no-default-features` drops the whole dependency chain for anyone who does
not. A plugin system that requires rebuilding the database before it can load a
plugin is not one.

The default flip costs binary size and build time — **measured, as promised,
once the weight was real**: the release `drsg` went 37.0 MB → 57.6 MB with
wasmtime trimmed to `runtime`, `component-model`, `cranelift`, `std`. Kept on
against that number: a fifth of the binary buys the entire plugin system, and
`--no-default-features` still drops it. Throughput through the sandbox, same
corpus as the native baseline: 8.6 MiB/s end-to-end against native's 23 —
this workspace in ~190 ms (7.5 after line provenance was added: span
tracking costs ~13%, paid knowingly — a fact you cannot jump to is half a
fact). The first measurement said 4.5 and was challenged
as kernel-inadequate; pre-linking the component at load, per-file `parse`
dispatch and a binary partial format closed it to ~2.7×, of which ~2.1× is
the wasm instruction floor (the single-document bench) and the rest the
serial `assemble` tail. The next lever, if a real tree ever demands it, is a
tree-reduce `assemble` — a contract change, deliberately not taken now. And the flip changes no security
posture: **the runtime being compiled in is not a plugin running.** No module is
loaded that the configuration did not name, so a default install executes
exactly as much third-party code as it does today, which is none.

The sandbox is the reason rather than a bonus. It is what makes a **third-party**
plugin viable at all, and it hands the trust decision to the operator instead of
asking them to take one on faith: a plugin starts with no filesystem, no
network, no environment and no clock, and receives exactly the capabilities the
configuration grants it. Two properties fall out that a subprocess cannot offer —
CPU is bounded by fuel metering or epoch interruption, so a runaway plugin is
*interrupted* rather than killed; and a plugin denied the clock and randomness is
deterministic, so re-ingesting a repository yields the same graph.

The protocol above is deliberately transport-independent, so a subprocess host
(for a plugin in a language that will not compile to wasm, or one that genuinely
needs the network) can be added later without the plugin contract changing.

**Settled — version travels inside `_generated_by`.** One property holding
`rust@1`, not a name and a sibling version: they are never useful apart.

**Settled — the plugin wins a conflict.** Where a parsed fact and a model entity
claim one key, the fact is kept and the model's is dropped and counted. A parser
knows where a model infers, and routing it to §8 stage 2 would spend a model
call re-litigating what the AST already settled. Edges are *not* deduplicated
this way: an edge carries no identity of its own, so dropping a relation the
model found because a parser found something between the same nodes would lose
real information.

**Settled — preprocessing is local-only.** The CLI and the **stdio** MCP server
route through it; `drsg serve` and the HTTP MCP server do not. What makes
parsing worth its cost is a plugin pulling the files *around* the one it was
handed, and that pull is exactly what a shared server must not offer — routing
it there would hand every caller a handler whose only reachable input is the
server's own filesystem. Text sent over the wire stays prose.

**Shipped (v2, slice 2) — the sandbox, and plugins you install.** Plugins are
wasm **components** against a WIT contract with two phases: `parse` turns one
chunk into an opaque partial (the host runs chunks in parallel — that is where
the cores are, and the guest stays single-threaded), and `assemble` turns every
partial into the result, once — cross-file resolution stays in the plugin
because it is language semantics and the database holds none. `drsg plugin
install <file.wasm | url>` validates the component, refuses one that imports
`wasi:filesystem`/`wasi:sockets` by name, pins its SHA-256 (re-checked at every
load — the *plugin identity* fork, settled), and records it in a per-user
store. A plugin reaches exactly `list`/`read`/`label`, rooted and resolved-path
checked; both wasi clocks are frozen; fuel and memory are bounded per call and
operator-settable — each guarantee proven against a committed hostile fixture.
The **Rust parser left this repository** for `dr-strange-extensions` (the
official extension repo: the language-neutral WIT, per-language SDKs starting
with `dr-strange-ext`, and the plugins), installed like anything else and
verified to produce the identical graph — 1324 nodes, 4204 edges on
`dr-strange-core/src` — to the native parser it replaced. Trees whose facts
exceed wasm32's 4 GiB address space are ingested a subtree at a time with the
plane as the accumulator: `apply()` is key-idempotent and keys are stable
qualified paths, so cross-subtree edges bind by exact key.

**Shipped (v2, slice 1) — the contract, natively.** `dr_strange_llm::preprocess`
holds `Preprocessed`/`Preprocessor`/`Host`/`Manifest`, the router
(`route_document` for one input, `route_tree` for a polyglot tree), grounding
(`FactsAndPlane`), the conflict rule (`fold`), and an in-tree Rust plugin built
on `syn`. Keys are module paths (`dr_strange_core::compute::exec::execute`), a
trait impl's methods are keyed by qualified path (`<T as From<i64>>::from`), and
calls resolve by locality — the caller's own module first, then each enclosing
one. What stays unresolved is counted in the report rather than guessed at.
`drsg digest <dir>` on a code-only tree writes a graph with **zero provider
calls**, which is the item's headline made real.

The wasm host is slice 2. It implements a trait that has already run in anger,
rather than one designed against a host that has never executed a module — and
the dependency weight wasmtime brings (Cranelift, `libc`, `object`, `rustix`)
deserves a deliberate re-look against the default-on decision above when it
lands, not a silent arrival.

**Forks settled by slice 2.** The wasm flavour is the **component model /
WIT**. Plugin identity is the SHA-256 pinned at install and re-checked at every
load. Determinism is enforced rather than requested: frozen clocks, fixed-size
chunking, partials assembled in chunk order.

**Shipped (v2, slice 3): the Go plugin — and what holding a second runtime
taught the sandbox.** `go@1` lives in the extensions repo beside the Rust
plugin: `go/parser` under TinyGo, the same parser/component split, 28 native
tests, and Go's own qualified names as keys (the module path from the nearest
`go.mod`, then `path.Ident`, then `path.Type.Method`). Interface satisfaction
is decided structurally under certainty rules — textual signatures within a
package, predeclared-only signatures across packages, an interface embedding
anything the tree does not declare left unmatched and counted. On a real
chain node (345 files, 6.2 MiB) it wrote 7.4k nodes and 13.8k edges in ~3.5 s
end-to-end, ~325 ms of that in the sandbox — and TinyGo's stdlib coverage
held: `go/parser`, `go/ast` and `encoding/json` all compile.

**Shipped (v2, slice 4): the TypeScript/JavaScript plugin.** `ts@1` claims
all of TS and JS (`ts, tsx, mts, cts, js, jsx, mjs, cjs`) — written in Rust
on `swc_ecma_parser`, one parser with a per-extension syntax switch, on the
proven wasip2 pipeline; the sdk/ts + componentize-js ecosystem proof is
deliberately its own later slice, so SDK risk never touches a flagship
parser. Keys are logical module identity (nearest `package.json` names the
package, `/index` collapses, `pkg/src/api.Client.connect` is a class
member); resolution covers named/default/namespace/aliased imports,
re-export chains through barrel files, `this.m()` lexically, JSX components
as calls — and **CommonJS**, because the first pure-JS corpus ingested had
524 `require()` sites and a graph with no imports at all: `require` is an
import wearing a call's syntax, `module.exports` is the export list, both
written down and both now read. `implements`/`extends` are syntactic in TS,
so those edges are certain where Go's had to be structural; `EXTENDS` joins
the vocabulary. 30 native tests; wasm == native exactly on a real OSS corpus
(zod: 3863 nodes / 6599 edges, notes identical); ~18 MiB/s through the
sandbox — the fastest of the three, as swc should be.

**Shipped (v2, slice 5): the Python plugin.** `py@1` (py, pyi, pyw), written
in Rust on ruff's parser — the same fork as TS, decided the same way. Module
identity is the language's own rule (`__init__.py` marks a package, the
first directory without one is the sys.path root, so `src/` layouts need no
special case); relative imports are package geometry resolved where the
module is known; dotted chains walk modules and stop the moment a step lands
on a value; `self.m()` resolves lexically; decorators are the calls they
are; class bases become EXTENDS; `__all__` is the written export list with
PEP 8's underscore rule as fallback, and ALL_CAPS sorts Const from Var
because PEP 8 says so. 21 native tests; wasm == native on psf/requests
(1049/1966) and on a 759-file agent codebase (21148/54632); ~21 MiB/s.
Four of five ecosystems now ship official parsers; Java remains, plus the
sdk/ts and sdk/py ecosystem-proof slices.

**Shipped (v2, slice 6): the Java plugin — all five ecosystems covered.**
`java@1` on tree-sitter's grammar, because no mature pure-Rust Java frontend
exists; the C runtime and grammar compile to wasm32-wasip2 under wasi-sdk's
clang, a toolchain investment that also opens the door to C and zig plugins.
Keys are Java's own qualified names (package, type, member — a file is not a
node); resolution reads references the way javac does (same package needs no
import, then imports, wildcards against the tree, `java.lang`); inherited
and `super` calls walk the in-tree extends chain; `ANNOTATED_BY` joins the
vocabulary because on a Spring codebase the annotations *are* the
architecture. 18 native tests; wasm == native on gson (1032/2600) and on a
1500-file Maven monorepo (7011/24599); deterministic; ~405 ms in the
sandbox for 3.4 MiB.

**Shipped (v2, slice 7): the C plugin.** `c@1` (.c, .h) on tree-sitter-c,
through the wasi-sdk toolchain the Java slice bought. The design is the
linker's model: external linkage is one flat namespace (a non-static
function's key is its bare name), `static` is file-local
(`filestem.name`), a header's declaration and its source's definition share
one key and the definition wins the node; a static shadows a global in its
own file, the compiler's rule. The preprocessor is recorded, not expanded —
`#define` becomes Const or Macro as written, include guards stay out,
`#ifdef` arms are walked so both platform variants are facts; `#include`
resolves same-directory then unambiguous-tail against the parsed set, and
ambiguity is counted because include paths are build configuration a parser
does not have. libc is external by name. 16 native tests; wasm == native on
redis/src (9633/34751, 2740 header declarations merged into definitions)
and on a CMake-built SDK (1746/5953); deterministic; ~15.5 MiB/s.

The sandbox had to *change shape* to hold it, in ways that were the slice's
real findings. A Go runtime imports `wasi:filesystem` before the plugin's
first line runs — as the Python and JS runtimes will too — so refusing that
import by name was refusing the toolchain, not the intent; the refusal moved
to where it is real: an **empty preopen table** (nothing to read, probe, or
enumerate), with `wasi:sockets` alone still refused at load because nothing
needs sockets to start. `wasi:random` now deals a **fixed byte sequence**,
because Go seeds map iteration order from it and real entropy would have
broken re-ingest determinism. And a trapped guest's stderr is **captured and
surfaced** in the error, because a Go panic prints there and a bare
"trapped" hid the whole diagnosis. Each guarantee is pinned by a hostile
fixture, as before.


**Shipped (v2, slice 8): the `git` plugin — history as a second plane.** A
checkout carries two sources of truth. The tree says what the code *is*; the
repository says how it got there. `git@1` reads the second one — commits,
branches, tags, merges, and the rebases only the reflog remembers — straight
out of `.git`, inside the sandbox, with **no `git` binary run**: it carries
its own reader for the object store (loose objects, v2 pack indexes, both
delta forms), refs (`refs/…` and `packed-refs`, loose winning) and the
reflog. 15 native tests, none of them a captured fixture — every one builds a
repository with the `git` binary, does something to it (merge, rebase,
`gc --aggressive`), and reads what git actually wrote; the packed and loose
readings of one repository must come out identical, property for property.
Measured on this repository: 429 commits, 11 merges, 15 tags in about two
seconds, end to end.

*Dispatch is by the shape of the source, not by an extension.* Every other
handler is chosen by a file's extension, because that is what its input is.
History is not a file, so `git` declares **no extensions** and the file
router never dispatches to it; the host runs it when the digested directory
turns out to have a git directory in it — something the host can see rather
than guess — and the plugin name `git` is the declaration, reserved the way
the official catalog reserves the others. Nothing is guessed: with no such
plugin installed, a digest simply does not read history and says so once.

*The grant is narrower, not wider.* It is handed a host rooted at the **git
directory** and nothing else: it cannot read a single source file, and the
tree's plugins still cannot read a single object — `.git` was always excluded
from the ordinary walk. Two planes for the same reason: `<plane>_git` beside
the code plane, because a code plane is a picture of the tree *now* and is
rewritten whenever a file changes, while history only ever grows. Folding one
into the other would mean re-reading a thousand commits every time a function
moved, and every `MATCH (n)` over the code would walk a graph mostly made of
commits.

*Writing is append-mostly, and shaped by what can change.* A commit is
immutable — its sha is its content — so one already in the plane is left
exactly as it is and its `PARENT` edges are never rewritten; only a moving
pointer (a branch, a tag, a rebase) is patched and has its edges re-asserted.
A second digest of an unchanged repository therefore writes nothing at all,
and a digest after one commit writes one node and two edges.

*Settled — the one thing that cannot be exact.* A rebase leaves **no trace in
the commit graph**: it writes new commits and moves a ref, and nothing in any
object records that one replaced another. The only record is the reflog,
which is local to one clone and expires (`gc.reflogExpire`, 90 days). So
`Rebase` nodes are reconstructed from it, carry `_from_reflog`, and the
report states the limit — an absent `Rebase` must never read as "no rebase
happened". The same reflog is why commits no ref can reach are kept and
marked `reachable: false`: they are what a rewrite left behind, and a graph
showing the rewritten branch without them could not say what changed.

Released as `git-v1.0.0` and pinned in the official catalog
(`sha256:ce50d72f…`), so `drsg plugin install` offers it like any other.

*Kept current, and readable.* `serve watch` refreshes the history plane at
startup and on every HEAD move — so `drsg init` bootstraps both planes, and a
commit that touched no file the code plane holds (an empty one, or one that
moved only something ignored) still lands, because it moved a branch. The
read side is one verb, `history`, on the CLI and as an MCP tool: HEAD, the
branch and tag tips, the rebases with what each replaced, and the newest
commits, with every listing saying what it is a listing *of*. `list_planes`
says which plane holds which repository's history, and the server's
instructions carry the vocabulary, so an agent does not have to discover the
schema before it can ask a question.

*A ref that moves without a commit lands too.* This was open on the grounds
that polling every ref would double the watcher's git calls — true, and beside
the point: refs are *stated* by the filesystem, so the same two-second tick
hashes each ref's size and mtime (plus `packed-refs`) and runs no git at all,
which is cheaper than the `rev-parse HEAD` it was already spawning. HEAD
unchanged but the fingerprint moved refreshes the history plane alone, since no
file changed and the code plane has nothing to do. `git tag v1` on a commit
already folded therefore reaches `<plane>_git` within one tick. Git writes a ref
by renaming a fresh temp file over it, so content and metadata move together;
the gap is deleting a ref and recreating it at the same sha inside one mtime
tick, which describes a plane that is already correct.

*The two planes are linked.* This was worded as an edge from a `Commit` to
the symbols it changed, which the storage layer rejects — planes are hard
namespaces (§3). The join is a read of both, the shape `deployed` already
uses, and the thread is a path: the git reader records the paths each commit
changed, a symbol records the file it is declared in, and `history <name>`
puts the two together. Tree diffing turned out to be affordable rather than a
different order of cost — git names a tree by the hash of its contents, so an
unchanged directory is one oid comparison however large it is, and recursing
only where subtree oids differ makes a commit cost what it changed rather
than what the repository holds. Measured on 1636 commits: 78ms to read the
history, 383ms with the paths, ~0.19ms a commit, 6202 paths. A merge records
only what differs from *every* parent — git's own `TREESAME` rule — so an
ordinary merge lists nothing and its side commits carry the changes.

The join is file-level and says so in those words, because the honest
symbol-level answer is to read the declaration at each of those commits and
the one before, which `recall <name> at <sha> vs <sha>~1` already does over a
list this verb has bounded to one file's history. Each way the answer can
fall short is a note of its own rather than a silent omission: a plane folded
before the reader wrote any paths, a commit whose own list stopped short, a
rename (git stores a tree, not a rename), and the commits a rewrite left
behind, which `git log -- <path>` never walks to.

*That comparison now runs inside the verb.* `history <name> --exact` reads
the declaration at each listed commit and at that commit's **first** parent
— the line the commit was made on, so a merge is compared against what it
was merged into rather than against the branch it absorbed — and marks each
row `← declaration changed`, `— file only`, `+ declared here` or
`− removed here`. It lives beside `recall` in the MCP layer, which is where
the checkout and the parsers are; core keeps the one renderer and takes the
marks back through a `Marks` struct, so the plain answer and the exact one
cannot drift into two different sentences.

Compared by the declaration's own lines, not by an edit script: the question
is whether it moved, and a longest-common-subsequence table is the price of
saying *how*, which this caller does not ask. It is capped at fifteen commits
— the listing's own default length — and says how many it read and how many
it left. Measured: ~300ms a commit on a thousand-line file, so 4.5s for a
full fifteen. That is why it is asked for rather than assumed, and why a
comparison that could not be made carries `?` with the reason instead of
counting as unchanged.

A tempting optimisation that is **wrong**: since the listing holds every
commit that touched the file, commit *k*'s parent appears to have the same
file content as commit *k+1* in the list, which would halve the parses. It
does not hold — the plane keeps the commits a rewrite left behind, so the
listing is not a linear ancestry, and the chain breaks exactly where this
section already takes care to be honest. Caching parses by revision instead
is always safe and measured at ~1.5% here, because a file's commits are
usually not parent and child.

*The bulk reading is the path form, and it now works.* `history <path>
--exact` marks each commit with the declarations that moved in that file —
`← call_names` against a ten-path commit, where the file-level listing could
only say the file was touched. One parse of a file yields every declaration
in it, so the whole file costs what one symbol in it cost: the twenty-symbol
framing this item first had was the wrong instance of the question, because
nobody asks it that way and a file is the unit a commit changes.

Three readings it keeps apart rather than collapsing into "nothing moved":
a file absent at one side means everything on the other arrived or left with
it; a file the parsers place no declaration in says that; and a file that
could not be read says which. An empty parse and an unchanged file look
identical otherwise, and only one of them is an answer.

*And the directory form, which was the last open question here.* `history
<dir> --exact` answers "what moved anywhere under this directory, by
commit": `history crates --exact` marks a commit `← 9  + 17  − 1  in 4 of 4
file(s)` and names the declarations beneath it, one line per file. Three
things had to be settled for it.

**One parse, not one per file.** The routing already took a *list* of paths
and buckets them by plugin, handing each its whole bucket — so the nine files
a commit touched cost the invocation one file costs, and the saving is the
wasm instantiation rather than the parse. `Parsers::parse` therefore takes a
set, and the single-file and single-symbol passes are the same reader asked
for one path. Measured over fifteen commits of `crates` (1054 of them match):
24 files read at two revisions each, and the pass itself is the same order as
the single-symbol pass over its own fifteen.

**What a name is, is measured, not guessed.** A plane holds files and
symbols, never the directories above them, so nothing can be *asked* whether
a name is a directory — and a directory the tree no longer has could not be
probed for either. So the scan collects both readings in its one pass: a path
the commits record itself is a file, and a path only ever recorded with
something beneath it is a directory. Git has no commit where both are true.
This also had to survive resolution: `src` fuzzy-matches `src/b.rs`, and a
name the code plane answered with *some other file's* path is not an answer
about that name — the directory above those files is.

**The output shape, which was the open question.** The commit's line counts
(`← 9  + 17  − 1  in 4 of 4 file(s)`) and the lines beneath it name, one per
file, through the same `Moves::mark` the single-file form renders. Two caps,
each bounding a different thing and both stated: eight files per commit
bounds the row, forty for the whole pass bounds the wait, and a path neither
opened is marked `? unread` rather than left to read as a path where nothing
moved.

One correctness note the real repository turned up, kept because it is not
obvious: a batch merges facts by key, so two files declaring one key — a
plugin bug by the digest's own rule — would leave one of them looking empty.
A file that comes back empty from a parse that *reported* a collision says
so, which a single-file parse could never have detected, because it never
meets the other declaration.

*Open, and the honest reading of that 383ms.* It is per **fold**, and it
grows with the length of the history, because the reader diffs every commit
it walks whether or not the plane already holds that commit's paths. A
commit is immutable, so this is pure repetition — the second fold of a
repository recomputes exactly what the first one wrote. The fix is not a
faster diff but a plugin that knows where to stop: the host would have to
tell a reader what the plane already has, which the preprocessor contract
has no way to say (input is pull-not-push, and a plugin sees files, never
the graph). Until then the cost is bounded by the two-second tick rather
than by anything principled, and a repository an order of magnitude older
than this one would feel it.

### The catalog is data, and it lives with the plugins  *(shipped)*

The official list started as a `const` here: nine entries, a release URL and a
pinned hash each. It read as a compatibility statement — *these artifacts are
known-good with this build's contract* — and that reading is what made it
wrong. It meant every plugin release was a release of this repository too: tag
`rust-v1.4.2` in the extensions repo, edit a Rust file here, bump, ship, for a
fact the host only repeats. The two projects release at their own pace on
purpose, which was the entire reason the plugins live apart; a list that
forces them to move together undoes it.

So the list moved to `catalog.json` beside the plugins, fetched over HTTPS and
cached in the plugin store, and the extensions repo's release workflow writes
each new version, URL and hash itself.

*Three forks, settled.* **Where it is read from** — the extensions repo's
default branch, so a release is a commit rather than a second publishing step.
**What happens offline** — every fetch is cached and a failed one falls back to
that copy saying how old it is; a cold cache with no network is an error naming
the URL, because nothing else is honest and a path or URL still installs with
no catalog at all. Nothing is vendored into the binary: a snapshot in this tree
is the very thing being removed, and one that quietly went stale would be worse
than an error that says so. **What replaces the compatibility statement** —
each entry carries `contract` (the WIT world it was built against) and
`min_drsg` (the oldest host it claims), and several entries may share a name so
a plugin can keep serving older hosts. An entry this build cannot run is listed
with the reason rather than filtered out: the wasm loader is the real gate, and
a plugin that silently vanishes from the list is a support question.

What did *not* move is the hash discipline. The catalog's `sha256` is checked
on download before the bytes are treated as a component, and the store still
pins what it installed and re-checks it at every load. Nothing compiles the
file any more, so the extensions repo's CI took over that job: it validates
every field and fetches each release's published checksum to confirm the pin.
---

## 12. Scoped identity — shared memory a team can actually run

**Goal.** Make one database safely reachable by an ops team *and* a fleet of
agents spread across machines, without the operational weight that usually comes
with multi-tenant auth. Concretely: LAN-reachable UI and RPC for maintenance,
`/mcp` reachable by agents on several hosts, per-agent identity that can be
revoked and attributed — and nothing that needs a separate server to run.

**Why AI-native.** §10 makes one memory reachable by many agents; this decides
*which* agent may read or write *what*. Shared memory without attribution is
shared memory nobody can trust: when two agent groups write the same plane, the
first question is always "which one wrote this, and was it allowed to?" A
memory layer that cannot answer that can be shared but not relied upon.

**The problem.** The v1 model is one shared bearer token that authorizes
everything (08 §4.1), plus an Origin guard that trusts loopback. That is right
for a desktop install and wrong the moment the listener leaves localhost. Worse,
the two interact: the zero-config fallback grants full write access when no
token is set and keys off *allowed origin* rather than *loopback*, so an
operator who adds a LAN UI origin — which a LAN deployment forces — and forgets
`DRSG_TOKEN` has published an unauthenticated writable database.

**Settled — scoped tokens, not signatures or an OAuth server.** The design is in
08 §4.2. `drsg_<keyid>_<secret>`, stored as `SHA-256(secret)` (high-entropy
random needs no password stretching), revoked by deleting a row, and carrying a
plane scope plus an `Access` tier. Per-agent request signing and OAuth 2.1 were
both considered and rejected: signing costs key distribution, rotation, clock
skew and a nonce cache while no off-the-shelf MCP client speaks a bespoke
scheme, and OAuth needs an authorization server. Behind TLS on a trusted LAN
neither pays for its operational weight. Revisit signing (via RFC 9421, not a
hand-rolled canonicalization) if `/mcp` ever faces the internet.

**Settled — planes are the isolation unit.** This answers §10's open isolation
fork. A team's agents get `Write` on their own plane, `Read` on a shared one,
nothing elsewhere. No new concept is invented: planes (09) already partition the
database and plane administration is already `Access::Admin`.

**Scope sketch.** Replace `SharedToken` with a `TokenStore` behind the existing
`Authorizer` seam — the trait was written for this and dispatch needn't change.
`Authorizer` starts returning a `Principal` instead of a `bool`, so writes can
be attributed in an audit log. `drsg serve` grows a second listener
(`--mcp-addr`) so the agent surface and the human surface can differ in network
exposure and credential type while sharing one `Arc<Database>`. Token issuance
and revocation are `Access::Admin` RPC methods plus a dashboard view.

**Forks to settle.**
- *Human credentials.* Do ops humans get the same tokens (paste into the UI), or
  a real login issuing an `HttpOnly` session cookie? The cookie is stronger
  against XSS; the token is less to build and less to run.
- *Bootstrap.* Issuing the first admin token on a fresh database, without a
  window where the server is open.
- *Scope granularity.* Plane-level is the obvious unit, but is label- or
  property-level scoping ever needed, or does that belong to a query-rewrite
  layer instead?
- *Audit storage.* A plane in the database itself (queryable, but writable by
  the thing being audited) or an append-only file beside it?
- *TLS posture.* Refuse a non-loopback bind without TLS outright, or warn? A
  refusal is safe and will annoy someone's internal test rig.

---

## 13. Projections & aggregation — a query that answers with a table  *(shipped)*

**Status.** ✅ Shipped (2026-09-02). A read can now return values rather than
records: `RETURN a.name, count(*) AS papers ORDER BY papers DESC LIMIT 10`
answers with columns and rows, on every surface.

- **A projection tail on the plan** (`LogicalPlan::project`) — named columns,
  each an expression or an aggregate, with its own `distinct`, `order_by` (by
  column index), `skip` and `limit`. `Option`, skipped when empty, so a plan
  that doesn't project serializes byte-for-byte as before.
- **Aggregates** — `count`/`sum`/`avg`/`min`/`max`/`collect`, each optionally
  `DISTINCT`, grouped by every column that isn't one (Cypher's implicit
  `GROUP BY`).
- **Earlier bindings are addressable** (`Expr::At`) — a row is a current node
  plus the trail that reached it, so `RETURN a.name, b.name` reads both ends of
  a hop, and `type(r)`/`direction(r)`/`id(n)` read the edge and the node id.
- **Every surface renders it the same way** — `{columns, rows}` from the MCP
  verbs, the RPC, `POST /cypher` and the CLI; a table panel in the dashboard.

**Fork settled — why this was "the most invasive change", and wasn't.** The
deferral rested on needing "a multi-binding row model … which ripples to every
surface". The linear pipeline turned out to have been carrying every binding
all along: a row *is* a path. What was missing was a term that names one, and
`Expr::At` is that term — resolution is pull, so a query naming no binding pays
exactly what it paid before.

The projection is a **tail**, not a `Step`, because it turns node rows into
value rows: nothing can expand or filter a node after it, and as a field that
is unrepresentable-otherwise rather than a rule the executor enforces. An
aggregate is likewise its own case in a column rather than an `Expr` variant —
it is a fold over rows, not a value a row has, which is what keeps
`WHERE count(*) > 3` unwritable instead of silently `Null`.

Every fold is total, like the rest of evaluation: nulls and values a fold
cannot use are skipped, so an aggregate over soft-schema data answers instead
of failing. `count`/`sum` over nothing are 0 and `avg`/`min`/`max` are Null,
because the mean of no values is unanswerable rather than zero.

**Follow-ups.** `WITH`-pipelining (a second pipeline over a projection's
output); cross-variable `WHERE` (`p.year < q.year`), which the same `Expr::At`
now makes expressible and only pushdown *placement* still holds back.

---

## 14. Recall — code as it was at any commit  *(shipped)*

**Status.** ✅ Shipped (2026-09-14). `recall` reads a repository's code as it
was at a git revision, over MCP and as `drsg recall`: a file, a line range, a
directory, or a symbol's declaration at that commit; a search of that tree
(`pattern`); or what changed in a name since an older revision (`vs`).

- **Revisions resolve over the history plane** (`core::rev`) — a sha or a
  prefix of four or more digits, a tag, a branch, a remote branch, `HEAD`, a
  date (`YYYY-MM-DD` or RFC-3339) and `<rev>@{<date>}`, each with `~n` / `^n`,
  walked over `PARENT` and its `order`. A ref outranks a sha prefix, as in git.
  A checkout with no `_git` plane falls back to git's own resolution and says
  so.
- **The bytes come from git's object store** (`llm::git::GitTree`) — the `git`
  CLI, run with no shell and with a caller's `GIT_DIR` cleared, over one
  commit's tree under the plane's root, served to the preprocessors as a
  `Host`.
- **A symbol is located by parsing its file as it was** — today's graph names
  the key and the file; the installed plugin parses that file at the commit;
  a file renamed since is followed back through `git diff -M`, and a key absent
  at that commit is reported with what the file did declare.
- **Diffs** — git's own for a file or a directory; declaration against
  declaration for a symbol, by a bounded longest-common-subsequence.

**Fork settled — why not the graph's own `AS OF`.** Time travel (§4) addresses
the database's commit sequences, not git commits, and a server keeps a bounded
window of them (twenty by default). The `_git` plane already holds every
commit from the first: it names the revision, and git's object store supplies
the bytes, however old.

**Follow-ups.** A `cat-file --batch` reader, should a plugin that pulls many
files make one process per read slow; `git grep --max-count` once git 2.38 is a
safe floor; `GitTree::read` serving any tracked blob while `GitTree::list`
filters dotfiles and ignored directories, so a plugin under `recall` can reach
a tracked `.env` that a digest would have passed over.

---

## 15. Edge properties a query can read — relationship variables in Cypher  *(shipped)*

**Status.** Shipped. An edge carries the facts a reader most wants to filter
on — `_resolved_by` / `_confidence` / `_ref` on a `CALLS` edge say how that
call was bound, `role` on `USES_TYPE` says whether a type is a field, a
parameter or a return, and the java plugin's Spring work (§11, issue #46) puts
an injection point's evidence and an annotation's arguments there as well — and
a query now reads all of them:

```text
MATCH (f:Function)-[r:CALLS]->(g:Function)
WHERE r._confidence < 1.0
RETURN g.name, r._resolved_by
```

**What this entry got wrong.** It was written as a query-engine feature, and
the engine already had it. The plan IR carried `Expr::At { binding:
Binding::Edge(hop) }` with `EdgeType` / `EdgeDir` / `EdgeProperty`, a per-hop
`resolve_bindings` deriving the traversal direction from the endpoints, and
builders re-exported at core's root — all of it reachable from `plane.query`
and the five SDKs the whole time. Only the Cypher front end never reached it:
the parser dropped the variable and the compiler refused the syntax. The work
was a parser and compiler job, not an engine one.

**Shipped.**

- Bind a relationship on a typed hop or an untyped one (`-[r]->`, `<-[r:S]-`),
  read `r.key`, `type(r)` and `direction(r)` (`"OUT"`/`"IN"` — a row's
  traversal is concrete even where the pattern asked for `-[r]-`).
- `RETURN r`: the whole edge as a map — `id`, `type`, `src`, `dst` and
  `properties` nested, so an edge property named `type` cannot shadow the
  relationship's. A map is an ordinary `PropValue`, so this needed no new
  result contract and no SDK regeneration, and `DISTINCT` / `GROUP BY` over it
  work through the existing total order.
- Cross-variable predicates (`WHERE r.w > a.x`, `WHERE a.name = b.name`),
  placed at the latest slot their variables name — the first point where the
  row has walked all of them, so a predicate still filters as early as it can.

**Forks, as settled.** A bound edge *is* a first-class value (the map above),
not only projectable properties. A variable may not repeat across a path,
matching openCypher — and is likewise refused on a variable-length hop, which
walks several edges, or sharing a name with a node, since a relationship is
addressed by which hop it is. Cross-variable predicates cost nothing in the
IR, which supported them already.

**Deferred.** Reading an earlier variable *across* a variable-length hop — see
the low-priority section. A `WHERE` that would need it is refused rather than
miscompiled; a projection of it is still wrong.

---

## 16. Routes across repositories — the hop a call graph cannot draw  *(shipped)*

**Status.** ✅ Shipped (2026-10-13), from issue #42. A page calls
`/api/v1/orders/42` and the handler is a controller in another language, often
in another repository, with nothing between them but a string — so the graph
held both files and no connection. `wire` matches the two sides when asked,
over MCP and as `drsg wire`: a path for both sides of it, a handler for who
asks, or the function making a call for what serves it.

- **A route is a fact, in one vocabulary.** The plugins record what the source
  states on the symbol that already exists — `serves` for a declaration,
  `requests` for a call, each `VERB /path/{}` with every variable segment
  reduced to `{}`, and `serves_as_written` / `requests_as_written` keeping what
  was written. Shipped in the Go, Java, Python, Rust and TypeScript plugins,
  with the convention itself in `dr_strange_ext::route` so a community parser
  states routes the same way.
- **Three shapes, not N frameworks.** An annotation carrying a path, a
  registration call whose name is the verb, and a client call with a path
  argument. The third needs no framework table at all — a path-shaped literal
  passed to *any* call is a route reference whatever the callee is named, which
  is the only way a project-local `request('/x')` wrapper is ever visible.
- **Nothing is drawn.** No `Route` node, no `SERVES` edge: a synthesised edge
  would freeze a guess, while a verb that matches on demand can say what it
  could not match — a request reaching no declaration, a declaration nothing
  asks for, a side that states no verb and so matches every verb on the path.
- **`attach` joins the graphs** (issue #43's half). An attachment is one plane
  property — a name, the plane it names there, and a URL when another drsg
  serves it — and **no token**: attached graphs are one environment, so a
  caller presents the token it already holds. `attach` / `detach` /
  `attachments` are CLI subcommands, MCP tools (so an agent wires two
  repositories together inside one session), and `plane.attach` / `plane.detach`
  / `plane.attachments` on the RPC, as plane lifecycle beside `plane.create`.
- **Crossing instances adds no protocol.** A plane another drsg serves is read
  over `plane.query`, the read method every drsg already serves, with the
  question travelling as a serialized `LogicalPlan` — so no RPC method and no
  SDK regeneration was needed for the federation itself. One plan serves both
  sides of the boundary, so a remote graph is asked exactly what a local scan
  reads and the two cannot drift.

**Fork settled — why the address guard stays on.** `dr-strange-web`'s fetch
guard refuses loopback and private addresses, which is exactly where a peer
drsg lives, and `serve --follow` sets the precedent of switching it off for
operator-named infrastructure. Instead each read re-permits exactly the host
its attachment names (resolving a hostname once, so no later answer to that
name can move the request), which costs nothing and still refuses a redirect
off that address — the way this class of thing escalates.

**Follow-ups.** A prefix the source does not state (an env var, a base
assembled at runtime) produces a route that matches nothing, counted as a
request reaching no declaration — honest, but it does not distinguish
*unresolvable* from *genuinely absent*; the plan's `route_unresolved` is
unbuilt. And the known misses are listed with the plugins — chi's `Route(path, fn)` prefix, gorilla's
`.Methods("GET")`, actix's `scope(…).service(h)`, a group handed to another
function, `@FeignClient`-style declarative clients, Spring servlet context
paths and gateway rewrites, and a literal segment where the declaration has a
variable.

**#43 built on this and changed none of it** — see §17.

---

## 17. Cross-repository dependencies — which repositories a change reaches  *(shipped)*

**Status.** ✅ Shipped (2026-10-14), from issue #43. Dozens of business
repositories depend on a shared library (`common-base-v2`, `agrivo-common`),
and "which of them would this change break" had no answer: a radius walked in
one plane stops at the repository boundary. `impact --across` answers it in the
other graphs too, over MCP and as `drsg impact … --across [attached|all]`.

- **Two keys, both already recorded — no plugin work at all.** A repository
  states a dependency twice: in its build, where the `deps` plugin keys it as
  its ecosystem names it (`com.acme:common-base`, with `version` and `scope` on
  the `DEPENDS_ON` edge), and in its source, where every code plugin mints an
  `External` stand-in under the key the reference wrote
  (`com.acme.common.BaseEntity`). Two handlers asserting the same foreign key
  agree rather than collide, so the stand-in in one repository and the
  declaration in another are the same key by construction. The whole feature is
  core-side: the plugins already wrote everything it reads.
- **The radius, not just the symbol.** The keys offered are the changed
  symbol's *and* every key its local radius holds, breadth-first — so a
  repository referencing a subclass of the changed class is found, which is the
  ordinary case and the one a seed-only match misses.
- **Two scopes, because two environments ask differently.** `--across` reads
  the planes this one attaches: explicit, and the only scope that reaches
  another drsg. `--across all` reads every plane this instance holds — the
  issue's actual environment, thirty-odd repositories on one machine under one
  drsg, which needs no setup because each repository already stated its
  relationships in its own build and imports. An empty plane and a `…_git`
  history plane are not graphs an answer names.
- **One query per graph.** Coordinates and symbol keys travel together in a
  single `SeekKeys` plan, which is what makes thirty graphs affordable; a
  radius wider than the cap is asked about its nearest keys and says so.
- **Nothing is drawn and nothing is stored**, as with `wire`: matched per call,
  so the answer can state what it could not read. Without `--across` the output
  is byte-identical to before — unlike `wire`, which crosses unasked because a
  route's two halves live in different repositories by nature, a radius *is*
  complete within one, and adding graphs changes what its total counts.
- **Three outcomes told apart.** A repository that declares the coordinate and
  references the radius; one that declares it and reaches none of it (a
  dependency it does not use, at whatever version it pinned); and one holding a
  key *without* `External`, which is this code again — vendored or digested
  twice — reported as `holds its own` rather than counted as a user.

**Fork settled — the Maven coordinate needs no bridge.** The deps plugin is
blunt that Maven and Gradle do not join the java parser's package keys:
`com.google.guava:guava` and `com.google.common.…` cannot be mapped to one
another without a guess. The earlier framing took that as the gap to close,
per library plane, by observing coordinate ↔ package-set inside one repository.
It is not needed. The question is asked *standing in the library*, whose own
`Manifest.declares` already **is** the coordinate it publishes, and a
dependent's manifest keys its declaration the same way. The coordinate is read
from this plane rather than mapped onto it, so the unmappability is sidestepped
rather than solved — and the deps plugin's documented gap stands untouched.

**Follow-ups.** The walk is still not federated: `impact` reads
`plane.neighbors` directly, so a cross-repository answer is rows per graph
rather than a traversal crossing the boundary, with `impact` run in that
repository for the inside radius. A graph another drsg serves reports which
keys it holds but not what references them or at which version, because
`plane.query` answers with nodes and a version is a fact about an edge — a
reader is handed the call that answers it there, and carrying edges across
would mean a new RPC method and SDK regeneration, which this did not pay for.
Versions are reported per repository but never compared, so "which repositories
are behind" is still the reader's arithmetic. Python's code join holds only by
luck (`requirements.txt` says `pillow`, the code says `PIL`), and a Gradle
version catalog (`implementation(libs.something)`) is skipped by the deps
plugin, so both leave the build join standing alone. Lockfiles are not read, so
a transitive dependency no manifest declares appears through the code join
only. And `--across` is on `impact` alone: `trace` and `fathom` do not cross.

---

## 18. One process, many databases — `mount`  *(shipped)*

**Status.** ✅ Shipped (2026-10-14), from issue #44. Thirty-four repositories,
thirty-four `drsg serve` processes, thirty-four ports (17700–17733): the
management cost and the memory are per process, and a global MCP config names
one port, so an agent reached one repository out of thirty-four. `drsg mount`
serves another database's planes on a running drsg, and every verb follows.

**One database with thirty-four planes would also have answered this**, and is
not what shipped. The stores already exist — months of java/TS/python parsing
and embedding — and re-digesting them produces what is already on disk. Three
further reasons mounts beat consolidation, and they are the design:

- **One writer slot per database.** A single store serializes every
  repository's `serve watch` fold behind one gate; thirty-four stores write in
  parallel.
- **Per-repository lifecycle.** Dropping a repository is an unmount, not a
  `drop_plane` range-delete inside a shared file.
- **Blast radius.** One corrupt store is one repository.

- **The mount table is core's, not a surface's.** `Instance` owns the host
  database and the mounted ones; resolution hands back an owned `Arc<Database>`
  because `PlaneHandle` borrows what it reads and the table changes while the
  process runs. Being in core is what lets everything that *enumerates* planes
  see mounts — which is why `impact --across all` (§17) answers across all
  thirty-four repositories in one process, with no attachment setup at all.
- **A plane name is resolved once per request.** `plane` is the one key every
  method and tool uses for "the plane to act on", so the RPC resolves it at
  dispatch and the MCP layer in one `run_in`; all 39 RPC methods and 26 tools
  answer across mounts without being touched. A prefixed mount's name is
  rewritten to what its own database calls it.
- **Plane names stay flat**, so `--plane svc-orders` works exactly as before.
  `startup` is skipped (every database has one, and it would clash on the first
  mount); any other clash refuses the whole mount and names `--as`.
- **`serve watch` watches every mounted repository**, on the tree its own plane
  records — the slice that actually retires the other thirty-three processes.
  One `Instance::on_mount` hook covers a mount replayed at boot, named on the
  command line, or arriving over the wire hours later.
- **Written down and replayed.** A mount lives in the host's `startup` plane
  properties, like `attachments`; a recorded directory that has moved is
  reported and skipped rather than taken as a reason not to start.

**Fork settled — who may name a mounted path.** `db.mount` opens a directory a
caller names, and a plane inside it records a `synced_root` that `grep` and
`snippet` would then read: the escalation `TreeAccess` exists to prevent. So
the admin tier is not enough on its own — the path must sit under one granted
directory (`[server] mount_root`, defaulting to the operator's home), the web
crate refuses mounting outright when given no grant, and that same directory
becomes a second readable root for the file-reading verbs. A local process
(stdio, the CLI) is unconfined, since it already runs as the user whose files
these are. Two things the engine settled for free: a store another process
holds is refused by its own exclusive lock, with a message that already says
`run drsg serve and share that instance instead`; and `mount` refuses a path
that does not exist, because `Database::open` creates on demand and a mistyped
path would otherwise be answered with a new empty database.

**Follow-ups.** `drsg snapshot` / `restore` and `db.stats` are the host's
database, not the mounts' — a mounted store is backed up where it lives.
There is no cross-mount transaction, and none is offered rather than faked.
`serve --follow` replicates one database, so a mounted instance is not a
replication unit. `drop_plane` takes a `name` and so stays on the host: a
mounted plane's name is simply not found there, which is safe but terse.
And a mount is read *and written* through the same handle, so
`digest --plane <mounted>` writes that mount's store — correct, and worth
saying out loud.

---

## Low priority (deferred — not first-class for now)

These are real graph-DB table stakes but explicitly **not** a current priority.

- ~~**Aggregation & projections in the query language**~~ — shipped, see §13.
  What is still deferred from that entry is **`WITH`-pipelining**: a projection
  is a tail, so a second pipeline over its output would be a genuinely new
  shape rather than another clause.

- **Constraints / schema validation** (uniqueness, required properties,
  edge-cardinality enforcement). The soft-schema catalog *observes* structure
  but doesn't *enforce* it. Nice for integrity; not a differentiator.

- **A pattern slot past a variable-length hop, read from a later slot.**
  `Expr::At` addresses the row's trail, while the compiler numbers pattern
  slots; a variable-length hop walks a number of edges that varies per row, so
  from there on the two disagree. `MATCH (a)-[*1..2]->(b)-[:E]->(c) RETURN
  b.name, c.name` therefore reports the node one hop along the trail rather
  than whichever node `b` matched — on an `a→b→c→d` chain the row whose hop
  took two edges says `b` where it means `c`. The compiler refuses this in a
  `WHERE` (§15) and a term read at its own slot is always right, so what is
  left is a projection or `ORDER BY` naming an earlier variable across such a
  hop. Fixing it means the executor learning the pattern→trail mapping, which
  is per-row: the plan would have to mark which expansions were
  variable-length and a `Row` carry its slot boundaries. Deferred because
  variable-length hops are mostly asked as reachability (`RETURN c`), where
  nothing is read off the middle of the path.
