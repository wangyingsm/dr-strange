# Plugins

When `drsg digest` walks a repository, it does not ask a model to guess at the
code's structure. Each source file is routed to a **preprocessor plugin** — a
sandboxed WebAssembly component that parses it with a compiler-grade parser and
returns **facts**: nodes and edges the parser is certain of. An AST does not
infer that `parse()` calls `lex()`; it knows.

The plugins live in their own repository,
[dr-strange-extension](https://github.com/wangyingsm/dr-strange-extension),
apart from the database on purpose: official does not mean lock-step. A parser
ships a fix without waiting for a database release, and the database releases
without waiting for eight toolchains. That repository is the extension commons —
the official plugins, the canonical contract, and the SDKs for writing your own
(the subject of the [Coding Agent](./coding-agent.md) chapter's second half).

## How a digest run uses plugins

The router assigns files by extension: every installed plugin claims a set
(`.rs`, `.py`, `.html` …), and any file no plugin claims falls back to the
built-in document reader and goes to the model as prose. What the claimed files
become is up to the plugin, in two phases:

1. **`parse`** — the host splits the routed files into fixed-size chunks and
   runs `parse` over them **in parallel**, one fresh sandbox instance per call,
   sharing nothing. Each call returns a *partial*: opaque bytes the host
   shuttles but never reads.
2. **`assemble`** — called **once**, with every partial in chunk order. This is
   where cross-file resolution lives — imports, headers, barrel re-exports,
   interface satisfaction — inside the plugin, because it is language semantics
   and the database refuses to hold any. The result must not depend on where
   the chunk boundaries fell.

Input arrives by **pull, not push**. A plugin is handed file *paths*, and reads
what it needs through the host — which is how a code parser follows an import
into a neighbouring file. A format with no cross-file structure can treat
`assemble` as concatenation; the SDKs provide exactly that as a default.

## The contract

The plugin ↔ host contract is one small [WIT](https://component-model.bytecodealliance.org/design/wit.html)
world, `drsg:preprocess@1.0.0`, canonical at the root of the extensions
repository and vendored by drsg (`just check-wit` fails when any copy drifts):

```wit
interface host {
  %list: func(suffix: string) -> result<list<string>, string>;
  read:  func(path: string) -> result<list<u8>, string>;
  label: func() -> option<string>;
}

interface preprocessor {
  describe: func() -> manifest;                          // name, version, extensions
  parse:    func(subject: input, options: list<tuple<string, string>>)
              -> result<list<u8>, string>;               // one chunk → an opaque partial
  assemble: func(partials: list<list<u8>>, options: list<tuple<string, string>>)
              -> result<output, string>;                 // all partials, in order → facts
}
```

The three `host` functions **are** the capability grant: a plugin can reach
exactly what is written there and nothing else. `%list` returns readable paths
under the digested root, *sorted* — unsorted directory order would vary the
output between runs, and re-ingesting a tree is meant to yield the same graph.
`read` returns one file's bytes, refusing any path that resolves outside the
root (the check is on the resolved path, so `..` and symlinks do not walk
through it). `label` names the input when its contents cannot.

A plugin's `output` is nodes, edges, prose, and a **report** — counts of facts,
prose characters, and skipped inputs, plus notes in words for whatever could
not be resolved. Counted and named rather than dropped silently: a thin graph
should be explained by its report, not investigated by re-running the ingest.
`options` carries the plugin's own settings from the operator's
`[plugins.<name>]` config section, passed through uninterpreted.

## No LLM in a plugin — the rule

A plugin never calls a language model. Not the official ones, not a
third-party one: the rule applies to every plugin, and it is **enforced by the
sandbox rather than requested of authors** — no network (`wasi:sockets` is
refused at load, by name), no environment, no filesystem beyond the three host
functions, so there is no way to reach a provider or the key a call would
need.

The division of labour is the point. A plugin's job is what a parser can
*prove*, deterministically; whatever genuinely needs a model — a doc comment's
meaning, a README, anything the parser cannot claim as fact — is returned as
**prose**, and the *host's* digest pipeline decides whether a model reads it,
under the operator's keys, budgets, and `--mode`. A repository that yields
only facts is digested with **no model call at all**.

The same line is kept visible in the graph itself:

- A parsed fact carries `_generated_by` (`rust@2`) instead of `_model`, so it
  is always distinguishable from a model's extraction. Where both claim one
  key, **the fact wins** and the model's claim is dropped and counted.
- Determinism is part of the contract, not an aspiration. The sandbox freezes
  the clock, deals entropy from a fixed sequence, and sorts directory
  listings — so digesting the same tree twice yields byte-identical facts.
  A model call inside a plugin would break exactly this.

## The sandbox

Every plugin is a `wasm32-wasip2` component running under a deny-everything
grant. A guest runtime may *import* `wasi:filesystem` — Go's does, before the
plugin's first line runs — but the preopen table behind it is empty;
`wasi:sockets` is refused at load, by name; clocks are frozen; entropy is
fixed; and each call runs under instruction and memory budgets. A trapped
guest's stderr is captured into the error the operator sees. Whatever a plugin
produces comes back as a **return value** — only the host writes to the
database.

The budgets are tunable in `drsg.toml`
([Chapter 2](./getting-started.md#configuration-file)):

```toml
[plugins]
fuel = 200000000000    # instructions per sandbox call (0 disables the check)
memory_mb = 3072       # linear memory per call, MiB (wasm32 itself allows at most 4096)

[plugins.rust]         # a plugin's own settings pass through untouched
include_source = true
```

One boundary follows from the pull model: preprocessing runs where the files
are. The CLI and the stdio MCP server route through it; bytes sent to a shared
`drsg serve` over the wire stay prose. The deliberate exception is
`serve watch`, where the operator points the server at a repository on its own
machine — an explicit filesystem grant — so commit folds run through the
installed plugins.

## The official catalog

Eight official plugins cover the common languages, each wrapping a mature
parser rather than reinventing one:

| Plugin | Claims | Parser underneath |
|---|---|---|
| `rust` | `.rs` | [syn](https://crates.io/crates/syn) |
| `go` | `.go` | Go's own `go/parser`, compiled with TinyGo |
| `ts` | `.ts .tsx .mts .cts .js .jsx .mjs .cjs` | [swc](https://swc.rs) — ESM and CommonJS |
| `py` | `.py .pyi .pyw` | [ruff](https://github.com/astral-sh/ruff)'s parser |
| `java` | `.java` | [tree-sitter-java](https://github.com/tree-sitter/tree-sitter-java) |
| `c` | `.c .h` | [tree-sitter-c](https://github.com/tree-sitter/tree-sitter-c) |
| `web` | `.html .htm .css` | tree-sitter html/css/js — one plugin, so `class="btn"` binds to the stylesheet defining `.btn` |
| `toml` | `.toml` | [toml](https://crates.io/crates/toml) |

Each releases at its own pace as a `<plugin>-vX.Y.Z` tag; CI builds the
component and publishes `<plugin>.wasm` with its SHA-256 on the
[releases page](https://github.com/wangyingsm/dr-strange-extension/releases).
drsg's binary pins the catalog — release-tag URL plus hash, the versions
known-good with that build's contract — and a bare `drsg plugin install` offers
it interactively. Install pins the artifact's SHA-256; every later load
re-checks it, so a file that changes on disk is refused rather than silently
run. Installing a name again is the upgrade path.

## What every official parser promises

The eight parsers are one family, built to one discipline:

- **Keys are the language's own qualified names** — `crate::module::fn`,
  `pkg.Type.Method`, `file.c::func`, `index.html#map` — never invented ids.
- **Everything carries its location**: a definition its `file` and `line`,
  an edge the line it is written on.
- **Resolved edges say how they were resolved**: each carries
  `_resolved_by` (which rule matched), `_confidence` (a band, not a decimal),
  and `_ref` (the text as written at the call site).
- **What cannot be known is counted, never guessed.** A call whose receiver
  type the source does not declare becomes an `UnresolvedRef` ledger entry
  with a `_reason` — queryable in the graph and rendered in `context` — rather
  than a plausible edge. A wrong edge sends an agent somewhere wrong; a
  missing one, honestly declared, sends it to `grep`.
- **External things are stand-ins**, keyed by the path as written and labeled
  `External`: "this code uses that" is recorded without pretending to have
  read code it never saw.

Those promises are what the [Coding Agent](./coding-agent.md) chapter builds
on — and what a community plugin should keep, which that chapter's second half
walks through.
