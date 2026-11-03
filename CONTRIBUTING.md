# Contributing to dr-strange

Thank you for considering it. This file is what a contributor needs in order
to send a change that passes on the first try: where things live, what the
gate checks, and the handful of conventions that are not obvious from reading
the code.

Everything here is enforced by CI, so nothing in this file is a matter of
taste. Where a rule exists because something once broke, the reason is given
— it is usually the fastest way to remember it.

## Where things live

| Module | Path | Scope |
|---|---|---|
| Engine | `crates/dr-strange-core` | Storage, cache, computation, the public API. Both backends live here behind features. |
| Query language | `crates/dr-strange-parser` | The openCypher subset: text in, a core plan out. |
| CLI | `crates/dr-strange-cli` | `drsg` itself — `init`, `digest`, `serve`, the agent verbs. The package is `drsg`, so `cargo` selects it as `-p drsg`. |
| MCP server | `crates/dr-strange-mcp` | The MCP surface over the engine. |
| Web | `crates/dr-strange-web` | `drsg serve`: JSON-RPC 2.0 backend, dashboard, change feed. Frontend in `crates/dr-strange-web/frontend` (Svelte 5 + Vite, built with bun). |
| LLM | `crates/dr-strange-llm` | Embeddings, digestion, entity resolution, `ask`. |
| Network | `crates/dr-strange-net` | Outbound proxy policy, shared by every crate that makes a request: the LLM client, the web fetcher, the MCP relay. |
| Logging | `crates/dr-strange-log` | Tracing setup for the binaries. |
| SDKs | `sdk/{typescript,python,go,java,c,zig}` | Six clients over the JSON-RPC surface. |
| Docs | `docs/en`, `docs/zh` | The mdBook, in two editions. |
| Benchmarks | `benchmarks/` | The engine comparison, the accuracy benchmark, the token-cost study. |

**Language plugins are not in this repository.** Every parser plugin — Go,
TypeScript, Python, Rust, Java, Kotlin, C#, C and the rest — lives in the
separate [`extensions`](https://github.com/wangyingsm/dr-strange-extension)
repository and ships as a wasm component through its own catalog. A change to
how a language is parsed belongs there; this repository only loads what the
catalog publishes.

## Before you start

- **Rust 1.95 or newer** (`rust-version` in the workspace manifest).
- **[`just`](https://github.com/casey/just)** — every check in this file is a
  recipe, and `just gate` is the whole of CI.
- **[bun](https://bun.sh)** for the web frontend, **[uv](https://docs.astral.sh/uv/)**
  for the Python SDK and the benchmark scripts.

```console
git clone https://github.com/wangyingsm/dr-strange && cd dr-strange
cargo build
just gate          # mirrors CI; run it before you open a PR
```

## The gate

`just gate` is the same set of checks CI runs, in the same order, with
`RUSTFLAGS="-D warnings"`. `gate-rust` first checks that your stable
toolchain is not behind the one CI installs, because a clippy lint added in a
newer stable is otherwise a green local run and a red PR.

| Recipe | What it covers |
|---|---|
| `gate-rust` | `fmt --check`, `clippy --all-targets`, the whole test suite, and a redb-backend pass |
| `gate-features` | Every optional-feature combination still compiles *and* tests |
| `gate-frontend` | `bun install --frozen-lockfile`, lint, build, and the frontend unit tests |
| `gate-docs` | `scripts/check_docs.py`, then both mdBook editions |
| `gate-hooks` | The usage-report watermark |
| `gate-sdk` | All six SDKs against a live server |
| `gate-supply` | `cargo deny`, `cargo machete`, `ruff` |

**While you work, you do not need the whole gate.** Run the tests your change
reaches — `drsg impact <symbol>` and `drsg context <symbol>` will tell you
which those are — and save the full gate for the end. On a large workspace
that is the difference between a few seconds and several minutes per commit.

Two things the gate will not catch for you:

- **CI runs plain `cargo test`, not `cargo nextest`.** nextest gives each test
  its own process, which hides races on process-global state; `cargo test`
  shares one. Prefer nextest while iterating, but run `cargo test` before you
  push, or a race will surface on the runner instead of on your machine.
- **Unset `DRSG_TOKEN` first.** The web HTTP tests and the gate both fail
  confusingly if that variable is exported in your shell, because the server
  then demands a bearer the tests do not send.

## Conventions

### Commits

One functional change per commit, split as small as it will go, with a short
subject line in the conventional style already in the log (`feat(core):`,
`fix(web):`, `docs(bench):`, `chore(ci):`). The changelog is generated from
these, so the type and scope are load-bearing. Resist bundling an unrelated
cleanup into a commit: it makes the change impossible to revert on its own.

### Comments

Every public type, trait, function, method and constant carries a doc
comment, as short as will do — one line wherever one line suffices.

Inline (non-doc) comments are rare by design. Write one only when it is
genuinely needed, keep it to a line where you can, and make it state the
*reason* the code exists rather than what it does. The code already says
what it does.

### Types

Prefer the newtype pattern over a bare `String` or `u64` wherever an
identifier has a meaning. When a parameter list passes five, introduce a
struct for it instead of adding a sixth.

### Tests

Tests live in inline `#[cfg(test)]` modules beside the code. A few notes that
have each cost someone an afternoon:

- `compact` is gated behind `json` in the core, so its tests need
  `--features json`.
- A backend-specific interface is gated at **compile time**, not by returning
  a runtime error. After splitting a gated function, run `just gate-features`
  — a `#[cfg]` is easy to lose in a refactor, and nothing else notices.

### Coverage

`cargo llvm-cov --workspace --summary-only` before a new milestone. The total
must not fall below the baseline recorded in the project's notes; a drop gets
fixed or explained, never waived in silence. Read it as a trend, not an
absolute — inline test modules count as covered, so the number flatters
production code.

### The API surface

**A new agent verb goes in MCP and the CLI, never in `openrpc.json`.** Every
method in that file is implemented by all six SDKs, so adding one there makes
six clients stale at once. The agent verbs (`context`, `snippet`, `grep`,
`history`, …) are deliberately outside it.

### Docs

`docs/en` and `docs/zh` stay at parity: a change to one gets the same change
in the other, in the same place. The register is technical — no
colloquialisms, no marketing. `scripts/check_docs.py` checks that every local
reference resolves and that the two READMEs still match in shape; it runs in
CI, so a broken cross-link fails the build rather than shipping.

## Sending a change

1. Branch from `master`.
2. Make the change, with tests, and run `just gate`.
3. Open a PR describing *what breaks without it* — the failure a reader could
   reproduce — rather than only what the patch does.
4. CI runs the gate across Rust, features, frontend, docs, hooks, all six
   SDKs and the supply chain. A red run is the finding, not an obstacle:
   read what failed before re-running it.

A change to the parsing of any language goes to the `extensions` repository
instead, where each plugin has its own test suite and its own release tag.

## Reporting a bug

What helps most, in order: the exact command, the version (`drsg --version`),
and what you expected instead. For anything to do with a plane being wrong,
`drsg catalog --plane <name>` says more than a description of the symptom —
it prints the soft schema the plane actually holds.

**Security issues do not go in the issue tracker** — see
[SECURITY.md](SECURITY.md).

## Maintainers

Every module above is maintained by [@wangyingsm](https://github.com/wangyingsm).
One name for all of them is a statement of fact rather than a structure: it is
who reviews and merges today, and saying so is more use to a contributor than
an empty table would be.

That is meant to change. A module here is handed over when someone has sent
enough changes to it to have opinions about where it should go — so the way
to become its maintainer is to send those changes, not to ask first. Parser
plugins are the easiest place to start, and they live in their own
repository: see [`extensions`](https://github.com/wangyingsm/dr-strange-extension).

Contributions are accepted under the same dual licence as the project
([MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option).
