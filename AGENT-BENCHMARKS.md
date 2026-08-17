# Agent benchmarks

[BENCHMARKS.md](BENCHMARKS.md) measures the engine; this document measures
the **agent surface**: how completely, and in how many round trips, a coding
agent can answer real structural questions over a codebase. Dr Strange is
compared against a plain ripgrep workflow and two open-source code-graph MCP
tools. As with the engine numbers: **indicative, not a leaderboard** — these
are our own task sets, run on one machine, and the competitors are moving
targets.

## Method

- **Corpora.** Two real repositories, identical for every tool: the
  dr-strange core crate (Rust, 53 source files) and a 760-file Python
  project (the `hermes` proxy). Both copied to scratch working trees with
  their git history.
- **Contenders.**
  - **drsg** — planes digested by the released parser plugins
    (`--no-embed`), served over MCP with the source tree attached.
  - **[codegraph](https://github.com/colbymchenry/codegraph)** v0.9.6 —
    indexed with `codegraph index`, queried through its MCP tools.
  - **[codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp)**
    — indexed with `index_repository` (fast mode), queried through its MCP
    tools.

  Every tool answered from its own pre-built index; indexing time is not
  counted.
- **Arms.** One Sonnet-class agent per (task × tool), told exactly which
  tools exist and required to produce a ledger: every claim with a
  `file:line` receipt, every gap declared. Prompts otherwise identical.
- **Metrics.** Tool calls and tokens. Wall-clock is noted where meaningful
  but the arms ran concurrently against one API, so latency is
  contention-shaped; the agent floor is ≈26k tokens per arm regardless of
  tool.

## Task 1 — callers, callees, and honesty

*"Who calls `WriteTxn::delete_node`, at which lines, and what in its body
could you not resolve?"* Ground truth: 11 call sites across 10 functions
(one inside a closure), and a body with 6 genuinely untypeable iterator
calls. The crate deliberately contains a same-named trap: two distinct
`delete_node` symbols.

| | calls | tokens | callers | honesty |
|---|---|---|---|---|
| drsg | **2** | 32.7k | **10/10 functions, with call-site lines** (the closure site is a recorded fact) | full ledger: 6 unresolved with reasons, plus the resolved list for contrast |
| codegraph | 3 | 35.2k | 10 functions, definition lines only | 3 body calls silently absent; two same-named symbols false-edged (`neighbors` → a cache-layer method, `push` → the wrong file); no completeness metadata |
| codebase-memory-mcp | 10 | 51.8k | 9 clean + closure site on a file node (disclosed) + one spurious self-edge on the qualified call | **zero outbound callees** recorded for the method's body; the gap is only discoverable by diffing against source |

## Task 2 — impact report

*"We are changing this function's signature — produce the complete impact
report."* Ground truth: 8 production callers, 11 direct test callers, 17
string-reference patch sites.

| | calls | tokens | result |
|---|---|---|---|
| drsg | **2** | 35.3k | complete — one `context` call for the graph facts, one `grep` call reconciling every count against the raw text |
| codegraph | 4 | 38.6k | 8 + 11, **0/17 patch sites** — its own words: "0 found by structural search, not 0 exist" |
| codebase-memory-mcp | 11 | 59.3k | complete — but the production edges carry confidence 0.38, and its own arm advised "manual grep re-check before the change" |

## Task 3 — flow trace

*"How does this test reach `edge::delete_edge`?"* Ground truth: a fully
static four-hop chain crossing test, API, and storage layers — and passing
straight through the same-named-symbol trap.

| | calls | tokens | how it went |
|---|---|---|---|
| drsg | 4 (2 doing the work) | 30.8k | first `trace` on the bare name returned candidates (by design); the exact-name retry returned the whole chain, with files and lines, in one call |
| codegraph | 9 | 41.1k | its trace tool failed with a false explanation — "breaks at dynamic dispatch" on a fully static chain (the real cause was same-name aggregation); the agent recovered the chain by seven calls of manual forensics |
| codebase-memory-mcp | 13 | 50.3k | the graph mis-bound one hop and recorded another as empty; the agent rebuilt the chain by reading source snippets at every hop — the answer came from the source, not the graph |

## Task 4 — compound audit

One task needing three tool kinds at once (Python corpus): a flow chain, a
complete literal census of one identifier (ground truth: 87 lines in 22
files), and a blast radius with a production/test split.

| | calls | tokens | chain | census | blast radius |
|---|---|---|---|---|---|
| drsg | **4** | 42.0k | exact | **87/87 lines, all 22 files**, cap-checked | 21 direct callers with prod/test split, 60 at depth 2 |
| codegraph | 5 | 38.0k | exact | **honestly declined** — 7 symbol locations vs 87 real lines, correctly self-diagnosed | 14 direct (7 test callers missing) |
| codebase-memory-mcp | 8 | 64.4k | exact, best qualitative detail | **82/87, silently scoped** — its internal grep only sees files its indexer kept, and fast mode had excluded two directories; the total still summed "exactly" to its own count | 8 direct (all test callers absent) |

The census cell is the sharpest contrast in the whole program: an answer
that is wrong *with a self-consistent receipt* is more dangerous than a
declined one, and both are worse than a complete one that names its own
bounds.

## Versus ripgrep

An earlier two-way round (same corpora, same arm design) compared the graph
surface against an agent driving ripgrep directly:

- Structural questions: one `context` call versus 3–5 rg calls, with the
  graph arm ≈2× cheaper in marginal tokens — an rg arm reconstructs
  callers/callees by reading matches; the graph already resolved them.
- On cheaper models the gap widens: resolution lives in the tool, so the
  drsg arm's quality held, while the rg arm's per-step judgment degraded.
- Literal text was rg's win at the time — the graph arm honestly declined.
  The `grep` verb (literal search over the watched source tree) has since
  closed that: task 4's census above is that verb at work.

Ripgrep still wins when there is no server attached: nothing to install,
works on uncommitted state, and raw literal search over a cold tree is its
home ground. drsg's `grep` needs a served plane with a source tree attached.

## What drsg does not win

- **Uncommitted state.** The watch is commit-driven: the graph answers for
  the code as committed, and says which commit. The working tree's unstaged
  edits are invisible until committed.
- **Recall is a lower bound, on purpose.** What the parsers cannot prove
  from declared facts goes to the unresolved ledger with a reason, not into
  an edge. Same-run benches showed both competitors "winning" cells with
  edges that turned out to be wrong; we consider a wrong edge worse than a
  missing one, and the bound is stated in-band.
- **Display caps can elide names.** In task 4, one depth-1 caller appeared
  as "…and 1 more" under the group cap — the count reconciled, the name
  cost a follow-up call.

## Cumulative verdict

Across the four tasks, drsg completed every cell at 2–4 tool calls and the
lowest marginal token cost, and was the only tool whose answers state their
own bounds. The recurring competitor failures were one family: same-name
discipline — aggregating or mis-binding identically-named symbols — plus
string blindness on the census. Both are exactly the cases the parser
family's rules (qualified-name resolution, declared-facts-only typing,
stamped edges, unresolved-ref ledger) were written to close.
