# CLI Tools Layer

**Status**: shipped — `drsg` (M4) and `digest`, the latter as AIgest's three
passes (ROADMAP §8), reading URLs (§9) and any office document (§7 here);
the agent verbs and plugin management landed with ROADMAP §11 ·
last revised 2026-09-14

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
drsg init [--dir D] [--rebuild]           # bootstrap a repo: digest, spawn `serve watch`, write .mcp.json
                                          #   (without the `digest` feature: create a database)
drsg plane list|create|drop|show          # plane lifecycle (09-planes.md)
drsg import <file> --plane P              # JSONL ingest (bulk writer)
drsg export --plane P                     # snapshot export
drsg get <id|@external-key> [--plane P]   # single record, with descriptions
drsg query [--plane P] <plan-json>        # run a serialized plan
drsg cypher '<stmt>' --plane P            # openCypher subset, compiled to a plan
drsg context|describe|trace|impact|fathom <name> # agent verbs over a digested plane
drsg snippet <name|path:a-b> [--root D]   # a symbol read to its end, or a range of a file
drsg grep '<text>' [--regex] [--path P]   # text search over the tree a plane was parsed from
drsg traverse <key> [--edge-type T]       # the neighbours a hop (or several) away
drsg search '<query>' --plane P           # semantic top-k (embeds the query)
drsg history [--plane P] [--limit N]      # the repository behind a plane: HEAD, tips, rebases, newest commits
drsg queries [<id>] [--limit N]           # the Cypher queries that have run; `cypher --history <id>` reruns one
drsg catalog [--plane P]                  # soft-schema view (labels, props, descriptions)
drsg algo … / drsg hybrid …               # graph algorithms; fused retrieval
drsg index ensure <label> <prop> --plane P --metric cosine
drsg vectorize --plane P                  # embed a plane for similarity search
drsg stats / drsg check                   # counters; integrity scan
drsg snapshot / drsg restore              # whole-database backup bundles
drsg serve [watch]                        # dashboard + JSON-RPC + MCP; watch keeps a code plane commit-synced
drsg ask '<question>'                     # NL → read-only plan → run
drsg plugin install|list|remove           # preprocessor plugins (07 §1)
drsg update [--bin B] [--dir D]           # hand this process to the installer when a newer release exists
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

## 4. Operational contract

What the binary promises beyond its command surface — each a line a
security or operations reader can hold the code to.

- **Self-update runs the installer the release shipped.** `drsg update`
  resolves the latest tag through the `releases/latest` redirect, then
  fetches `scripts/install.sh` *at that tag* — never from `master` — and
  passes `--version <tag>` so the installer installs the release the check
  decided on rather than resolving "latest" again. Every interpolated value
  (`--bin`, `--version`, `--dir`) is single-quoted for `sh -c`. The
  installers (`scripts/install.sh`, `scripts/install.ps1`) require the
  archive's `.sha256` sidecar: missing, malformed or mismatching is a hard
  failure, as is having no tool to hash with. `--insecure-skip-checksum`
  (`-InsecureSkipChecksum`; `DRSG_INSECURE_SKIP_CHECKSUM=1` in every
  surface, including `drsg update`) is the one escape hatch and warns.
  The sidecar shares the archive's origin, so this is integrity against a
  truncated download, a stale mirror or a swapped asset — not a signature.
- **Where `init` puts the token.** `.mcp.json` and `.cursor/mcp.json`
  carry the bearer token literally (a desktop client has no shell
  environment to read one from) and both are in the `.gitignore` block
  `init` maintains — the invariant is *every file written with a literal
  token is in that block*, pinned by test. `.opencode.json`,
  `.gemini/settings.json` and `.codex/config.toml` pre-exist `init` and are
  probably committed, so they receive only an environment reference in the
  client's own syntax (`{env:DRSG_TOKEN}`, `$DRSG_TOKEN`,
  `bearer_token_env_var`); `init` prints the export line.
  `drsg.example.toml` ships with `token` commented out. `[server] token`
  and every `[llm]` key are applied to the process environment at startup
  (never overwriting a variable already set) and are therefore inherited by
  every child process — the spawned `serve watch` included — which the
  example file and Chapter 2 say in so many words.
- **Hooks.** `init` repoints a Claude Code hook entry only when its command
  is a bare path whose last component is exactly one of drsg's script
  names; anything else in `settings.local.json` is someone else's and is
  left alone. The shell guard decides "this is a write, let it through" on
  the command with quoted and backslash-escaped text removed and numbered
  fd redirects dropped, so a pattern containing `>` or `<<` does not bypass
  it. The repository's own `.claude/settings.json` runs `init` with
  `DRSG_ENSURE_UNCONDITIONAL=1` on purpose — this repository is drsg — and
  the hook script's header says why a copy elsewhere should not.
- **Retention is applied wherever the database is opened.** `[server]
  retain_commits` (default 20; `0` = unbounded) is read by
  `config::retain_commits` and set on the handle by `commands::open`, which
  every CLI command goes through — not only `serve`. A CLI-only store
  therefore reclaims old versions at compaction like a served one. The two
  `init` opens that only create the file pass no retention: nothing is
  written through them, and the `serve watch` they spawn sets its own.
- **Respawn.** A port `init` picked itself is retried (up to three picks)
  when the child exits before listening — the pick-then-bind gap is a race
  it can lose; an explicit `--addr` or a recorded address is never swapped.
  "Listening" means the child itself answers `/health` with its own pid: a
  stranger that won the port and accepts connections is not taken for the
  child, so `init` never reports success against a server it did not
  start.
  Before SIGTERM-ing the pid a `/health` body named, `stop_server` checks on
  Linux that `/proc/<pid>/cmdline` is a drsg binary and refuses otherwise;
  elsewhere the check passes.
- **The usage-report hook** keeps its per-session watermark under
  `$XDG_RUNTIME_DIR/drsg` (else `~/.cache/drsg`, created 0700), written
  through an `O_EXCL` 0600 temporary and renamed — never in the shared
  temp directory under a predictable name.

## 5. Open questions

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
