# CLI Tools Layer

**Status**: draft · `drsg` built (M4), `digest` deferred · 2026-07-28

**M4 landed** the `drsg` binary (clap): init, plane list/create/drop/show,
import/export (JSONL in the `json` dialect below), get (id or
`@external-key`), query (a serialized `LogicalPlan` as JSON), catalog, index
ensure, stats, check. Handlers are testable functions over the core API
writing to a `Write`. The JSON dialect lives in `dr-strange-core`'s
feature-gated `json` module (shared with MCP). **`digest` is intentionally
absent** pending its own design session (§3, arch/07).

Scope: the `drsg` binary (`dr-strange-cli` crate) — the human-facing command-line
wrapper over `dr-strange-core`. First consumer of the public API; its job is equal
parts utility and **forcing the API to be ergonomic early**. Contains no
database logic.

## 1. Command surface (v1)

```
drsg init <path>                          # create a database
drsg plane list|create|drop|show          # plane lifecycle (09-planes.md)
drsg import <file> --plane P              # JSONL/CSV ingest (bulk writer)
drsg digest <doc>... --plane P            # LLM-powered document → graph ingest (§3)
drsg export --plane P [--format jsonl]    # snapshot export
drsg query [--plane P] <builder-json>     # run a serialized plan; table/JSON output
drsg get <id|@external-key> [--plane P]   # single record, with descriptions
drsg catalog [--plane P]                  # soft-schema view (labels, props, descriptions)
drsg index ensure <label> <prop> --plane P --metric cosine
drsg stats                                # cache hit rates, sizes, counters
drsg check                                # integrity: KV invariants, sidecar freshness
drsg bench <suite>                        # micro/traversal/hybrid benchmarks (M5)
```

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

> **Deferred**: the detailed design (extraction prompting/schemas, chunking,
> incremental re-digest, dedup against existing planes, cost controls) will
> be discussed separately and land in [07-llm.md](07-llm.md).

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
