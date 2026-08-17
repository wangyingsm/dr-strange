# CLI Tools Layer

**Status**: shipped — `drsg` (M4) and `digest`, the latter as AIgest's three
passes (ROADMAP §8), reading URLs (§9) and any office document (§7 here);
the agent verbs and plugin management landed with ROADMAP §11 ·
last revised 2026-08-18

**M4 landed** the `drsg` binary (clap): init, plane list/create/drop/show,
import/export (JSONL in the `json` dialect below), get (id or
`@external-key`), query (a serialized `LogicalPlan` as JSON), catalog, index
ensure, stats, check. Handlers are testable functions over the core API
writing to a `Write`. The JSON dialect lives in `dr-strange-core`'s
feature-gated `json` module (shared with MCP). `digest` was absent at M4
pending its own design session; that session happened and it shipped (§3).

Scope: the `drsg` binary (`dr-strange-cli` crate) — the human-facing command-line
wrapper over `dr-strange-core`. First consumer of the public API; its job is equal
parts utility and **forcing the API to be ergonomic early**. Contains no
database logic.

## 1. Command surface (current)

```
drsg init <path>                          # create a database
drsg plane list|create|drop|show          # plane lifecycle (09-planes.md)
drsg import <file> --plane P              # JSONL ingest (bulk writer)
drsg export --plane P                     # snapshot export
drsg get <id|@external-key> [--plane P]   # single record, with descriptions
drsg query [--plane P] <plan-json>        # run a serialized plan
drsg cypher '<stmt>' --plane P            # openCypher subset, compiled to a plan
drsg context|describe|trace|impact <name> # agent verbs over a digested plane
drsg search '<query>' --plane P           # semantic top-k (embeds the query)
drsg catalog [--plane P]                  # soft-schema view (labels, props, descriptions)
drsg algo … / drsg hybrid …               # graph algorithms; fused retrieval
drsg index ensure <label> <prop> --plane P --metric cosine
drsg vectorize --plane P                  # embed a plane for similarity search
drsg stats / drsg check                   # counters; integrity scan
drsg snapshot / drsg restore              # whole-database backup bundles
drsg serve [watch]                        # dashboard + JSON-RPC + MCP; watch keeps a code plane commit-synced
drsg ask '<question>'                     # NL → read-only plan → run
drsg plugin install|list|remove           # preprocessor plugins (07 §1)
drsg digest [<src>] --plane P             # document/repo → graph ingest, dry-run by default (§3)
```

(The M5 benchmark suites moved out of the binary: criterion micro-benches and
the cross-engine harness live in `benchmarks/` — `just benchmark` /
`just bench-compare`.)

## 2. Design notes

- **Query input is the serialized plan format** (the same `Expr`/plan
  serialization the wire protocol will use) rather than a bespoke CLI
  mini-language — no throwaway parser, and it doubles as the plan format's
  first round-trip test. The v2 query language slots in here later as
  `drsg query 'MATCH ...'`.
- Import formats: JSONL of `{labels, external_key?, properties}` node lines
  and `{src_key, dst_key, type, properties}` edge lines; property values may
  be `{"$desc": "...", "$value": ...}` to carry `PropDesc` descriptions. CSV
  with a column-mapping flag for tabular sources.
- Output: human tables by default (TTY), `--json` for scripts; descriptions
  shown with `--verbose`, elided otherwise.
- Exit codes map from `dr_strange_core::Error` variants; `drsg check` is the harness
  used by crash-recovery tests.

## 3. `drsg digest` — LLM-powered ingestion

With an LLM API key provided (flag/env/config), `digest` asks the model to
parse documents into the graph: extract entities and relations, emit nodes,
edges, and `PropDesc` descriptions, embed text for vector properties, and
write the result through the bulk writer — by default into a fresh plane per
document (the plane model's intended usage, [09-planes.md](09-planes.md)).

- All model interaction is delegated to `dr-strange-llm`
  ([07-llm.md](07-llm.md)); the CLI contributes argument parsing, document
  loading, progress reporting, and the write path.
- Rough shape (subject to the detailed design below):
  `drsg digest paper.pdf --plane auto --api-key ... [--model ...]
  [--dry-run]` — `--dry-run` prints the proposed subgraph without writing.

**Shipped**, and the detailed design lives in [07-llm.md](07-llm.md) and
ROADMAP §8: three passes (extract, reconcile, refine) with the mode chosen per
run, chunking that respects paragraph and document boundaries, dedup against a
plane's existing entities, and a URL reader (§9). A document may be any office
format, not only text — see 07 §1.

## 4. Open questions

1. ~~Should `drsg query` accept a convenience syntax pre-v2, or stay
   plan-JSON-only until the real QL?~~ **Moot — the QL landed.** `drsg cypher`
   runs an openCypher-subset statement compiled to a plan (ROADMAP §7), which
   is the shell one-liner the stopgap syntax was for. `drsg query` keeps taking
   plan JSON, for generated plans and for debugging the compiler's output.
2. Watch/REPL mode (`drsg shell`) — worth it in v1, or wait for the QL?
3. ~~Import dedup policy flag (`--on-conflict skip|update|error` by external
   key) — decide with the first real ingest corpus.~~ **Resolved: shipped, and
   it was a correctness fix rather than a convenience.** `bulk_load` is a
   trusting fast path — it rejects duplicates *within* a batch but does not
   check keys already in the plane — so an unguarded re-import wrote a second
   node under the same external key. The copy was reachable by scan, invisible
   to `key(n) = …` (which resolves through the index to exactly one node), and
   `drsg check` reported the database healthy: the same silent-divergence
   signature as the multi-process bug fixed in v1.4.2. `error` is the default
   because a colliding key almost always means the same file was imported
   twice. Under `skip`/`update` the file's edges still resolve to the node
   already in the plane; edges carry no key, so the policy governs node
   identity only and they are always appended.

   Every other path that feeds `bulk_load` untrusted keys is now guarded the
   same way — `digest.write` over `/rpc`, and `DigestResult::apply`, which
   covers both `drsg digest` and the MCP `digest` tool. Those skip and report
   the keys rather than refusing, because an extraction proposes every entity
   as new: naming something already known is the normal case there, where a
   colliding *import* key means the file went in twice. The MCP `write_nodes`
   tool was never affected — it goes through `create_node_with_key`, which has
   always rejected a taken key.

   The check stays at the callers rather than inside `bulk_load`: only paths
   taking untrusted input pay the lookup per key, and the fast path — a
   headline benchmark — keeps its trusting contract for callers that have
   already guaranteed fresh keys.

   Worth knowing about the failure mode, since it is worse than "a duplicate
   node": `bulk_load` writes the external-key index unconditionally, so a
   colliding key *overwrites* that entry. The original node stays in place but
   becomes reachable only by id, and every `key(…)` read against it silently
   returns empty.
4. ~~`digest` detailed design — deferred (see §3).~~ **Resolved: shipped** as
   AIgest's three passes (ROADMAP §8), extended to read URLs in §9.
