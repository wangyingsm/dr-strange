# Coding Agent

The [previous chapter](./plugins.md) described how parser plugins turn a
repository into facts. This chapter is about what those facts buy a coding
agent — and, for the languages the official catalog does not cover, how to
build a plugin of your own.

Four commands take a repository to a queryable, commit-synced graph:

```console
# Install parser plugins: a name from the official catalog, no argument for
# an interactive chooser over it (0 = all), or any .wasm path or URL.
$ drsg plugin install

# Digest a repository into a plane named after it
$ drsg --db codes.drsg digest ~/src/myrepo --apply --no-embed

# Serve the API + MCP surface and keep the plane synced to every commit
$ drsg --db codes.drsg serve watch --dir ~/src/myrepo

# One symbol's whole neighborhood, one call
$ drsg --db codes.drsg context 'WriteTxn::delete_node' --plane myrepo
```

`--no-embed` skips embeddings — parsing needs no model. Run `drsg vectorize`
later to make the plane semantically searchable.

**`drsg init`** collapses the digest-and-serve steps into one command, run
from the repository itself once its plugins are installed: it digests the
working directory into a plane named after it, spawns `serve watch` detached
on a freshly-picked address and bearer token, and writes `.mcp.json` — Claude
Code's own convention, also read as-is by GitHub Copilot. It then writes a
matching MCP config for Cursor, OpenCode, Gemini CLI, or Codex CLI, but only
for a tool whose own marker (a directory it creates, or a config file it
already owns) is already present in the repository.

```console
$ drsg init
plane 'myrepo' bootstrapped — serve watch pid 48213, http://127.0.0.1:51900/mcp
  + wrote ./.mcp.json
  + Cursor: wrote ./.cursor/mcp.json
```

**Run `drsg init` again whenever the server is gone.** It spawns `serve
watch` detached and records that process's address and bearer token in
`.mcp.json`, but nothing ever restarts it: an MCP `http` entry tells a client
where to connect, not what to launch, so no agent relaunches it and it does
not survive a reboot, a crash, or a kill. Re-running `init` is the way back,
and it is safe at any time — it probes the recorded endpoint first (`GET
/health`, which the server leaves unauthenticated for exactly this), leaves a
live server alone without opening the database, and restarts a dead one on
the *same* address and token, skipping the re-parse because the plane resumes
from its recorded commit.

```console
$ drsg init                       # already up: nothing to do
drsg is already serving . at http://127.0.0.1:51900/mcp — reusing it, the plane is untouched

$ drsg init                       # after a reboot: same address, same token
plane 'myrepo' restarted — serve watch pid 51002, http://127.0.0.1:51900/mcp
```

Leaving the database closed on the reuse path is what makes this safe to run
against a server that is already up: one process at a time may open the
plane, so an `init` that opened it to check would be the very collision it is
meant to avoid. Every agent's configuration stays valid across a restart,
because the address and token are recovered from `.mcp.json` rather than
generated afresh — and if that recorded port has since been taken by another
process, `init` moves to a free one and says so. Pinning `addr` and `token`
under `[server]` in `drsg.toml` fixes the endpoint outright.

So: once per repository, then again whenever the server is gone.

## What the facts buy an agent

An agent working from grep reconstructs structure on every question: search,
open files, read, infer who calls what, repeat. A digested plane has already
done that work, once, with a parser — so structural questions become **one
round trip** instead of a search-and-read loop.

Eight verbs carry the workload, identical over MCP
([Chapter 8](./mcp.md)) and the CLI ([Chapter 7](./embedded-cli.md); `grep`
and `snippet` read the watched source tree, so they live with the server):

| Verb | The question it answers |
|---|---|
| `context` | everything about one symbol — definition, callers with call sites, callees, references — the primary verb |
| `search` | "I don't know the name": semantic top-k over the plane's embeddings |
| `describe` | one symbol's properties — the lightweight node-only view |
| `grep` | text over the watched source tree — literal or regex, path-scoped, with context lines; each hit names the symbol it falls in; bounded and counted |
| `trace` | how one symbol reaches another: the shortest recorded call path |
| `impact` | blast radius: everything reaching a symbol, grouped by distance |
| `fathom` | what kind of place a symbol sits in: the region within a few hops, by label and edge type, with its hubs |
| `snippet` | a symbol's source text, or a range of a file (`path:start-end`) — the `sed -n` an agent no longer needs |

Every answer is compact one-fact-per-line text, sized for a model's context
window rather than a terminal, and `context` keeps itself within a fixed
budget by tightening its per-group caps and saying what it elided.

Under `serve watch`, the graph tracks every commit: changed files re-run
through the plugins and the plane is updated in place — new symbols created,
gone ones deleted, edges rewritten — converging on exactly what a full
re-digest would build. Each answer opens with `synced: commit <sha>`, so an
agent knows *which* code it is reasoning about; the working tree's uncommitted
edits are invisible until committed, and the answer says so by naming the
commit.

## Honesty is the load-bearing feature

An agent acts on what a tool tells it, so the family rule — *a wrong edge is
worse than a missing one* — shapes every answer:

- **An ambiguous name is never guessed at.** Two symbols named `delete_node`?
  The reply is the candidate list, and the exact-key retry costs one call —
  cheaper than one wrong answer confidently followed.
- **A call listing is a stated lower bound.** What the parser could not
  resolve is present as `UnresolvedRef` entries with reasons, rendered right
  in `context` — so "who calls this" comes back with the unresolved residue
  attached instead of silently shortened.
- **The graph names its blind spots in-band**: the honesty footer, the
  `synced:` line, elision counts. An agent can decide for itself when to fall
  back to `grep` — and the `grep` verb is on the same surface, so the
  fallback is one more round trip, not a change of tools.

In benchmarks against a ripgrep workflow and two open-source code-graph MCP
tools — same corpora, same tasks, one tool per agent — this combination
completed every task shape (callers, impact, flow, a compound audit) in 2–4
tool calls at the lowest marginal token cost, and was the only tool whose
answers state their own bounds. Methodology and full tables:
[AGENT-BENCHMARKS.md](https://github.com/wangyingsm/dr-strange/blob/master/AGENT-BENCHMARKS.md).

## Building a plugin for a new language

The plugin system is open: a community parser built against the SDK installs
and runs in the same sandbox as an official one, under the same
[contract](./plugins.md#the-contract) and the same
[no-LLM rule](./plugins.md#no-llm-in-a-plugin--the-rule). The pattern every
official plugin follows — and the strongest advice for a new one — is to wrap
a **mature, ideally canonical parser** (syn, swc, ruff, tree-sitter) rather
than write one, and to keep the parser a plain native library under a thin
component wrapper, so its tests need no wasm toolchain at all.

### Rust

Depend on the SDK and implement either the two-phase contract or, for a
format without cross-file work, the one-function facade:

```toml
[dependencies]
dr-strange-ext = { git = "https://github.com/wangyingsm/dr-strange-extension" }

[lib]
crate-type = ["cdylib"]
```

```rust
use dr_strange_ext::{Input, Manifest, Output, OutputExt, Simple, host, node, output, simple_plugin};

struct MyPlugin;

impl Simple for MyPlugin {
    fn describe() -> Manifest {
        Manifest { name: "mine".into(), version: "1".into(), extensions: vec!["xyz".into()] }
    }

    /// One subject at a time; the SDK derives parse/assemble from this.
    fn process(subject: Input, _options: &[(String, String)]) -> Result<Output, String> {
        let mut out = output();
        if let Input::Files(paths) = subject {
            for path in paths {
                let bytes = host::read(&path)?;
                out.nodes
                    .push(node(&path, "Thing").prop("bytes", bytes.len() as i64).build());
            }
        }
        Ok(out.finish())
    }
}

simple_plugin!(MyPlugin);
```

```console
$ cargo build --release --target wasm32-wasip2
$ drsg plugin install target/wasm32-wasip2/release/my_plugin.wasm
```

A real language parser implements the generated `Guest` trait directly —
`parse` returns a serialized partial per chunk, `assemble` resolves across
all of them — and pulls neighbouring files through the host bindings. The
official `plugins/rust` is the worked example.

### Go

Implement the `ext.Plugin` interface and build with TinyGo (≥ 0.41, with
`wasm-tools` on the `PATH`):

```go
package main

import ext "github.com/wangyingsm/dr-strange-extension/sdk/go"

type mine struct{}

func (mine) Describe() ext.Manifest {
    return ext.Manifest{Name: "mine", Version: "1", Extensions: []string{"xyz"}}
}

func (mine) Parse(subject ext.Subject, options map[string]string) ([]byte, error) {
    // Pull files via ext.List / ext.Read; serialize your partial.
    return []byte{}, nil
}

func (mine) Assemble(partials [][]byte, options map[string]string) (ext.Output, error) {
    return ext.Output{Nodes: []ext.Node{{Key: "k", Label: "Thing"}}}, nil
}

func init() { ext.Register(mine{}) }
func main() {}
```

```console
$ tinygo build -target=wasip2 -scheduler=none -gc=leaking \
    --wit-package ./wit --wit-world drsg:preprocess-build/plugin-go -o mine.wasm .
```

The flags are load-bearing (the extensions repository's `justfile` explains
why), and one rule runs through the Go SDK: copy everything lifted from the
ABI before use — a `cm` slice is a view the collector can move out from under
you.

### What a good parser keeps

The [family promises](./plugins.md#what-every-official-parser-promises) are
conventions, but an agent's trust rests on them, so a new plugin should keep
every one: the language's own qualified names as keys, `file`/`line` on every
fact, resolution stamps on every edge, the unresolved ledger instead of
guesses, `External` stand-ins for code outside the tree. Test the parser
natively against real source from the language's ecosystem; run
`just check-wit` before building, so the vendored contract copies match the
canonical one.

To offer it to everyone: contributions start as an issue on the
[extensions repository](https://github.com/wangyingsm/dr-strange-extension)
naming the parser you would build on, and land as
`plugins/<name>/{parser,component}` with a native test suite — CI builds
every component and runs every suite on each push.
