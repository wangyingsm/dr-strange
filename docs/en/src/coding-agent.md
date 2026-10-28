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
matching MCP config for Cursor, OpenCode, Gemini CLI, Codex CLI, Trae or
Kimi Code, but only for a tool whose own marker (a directory it creates, or a
config file it already owns) is already present in the repository or on this
machine. Qoder and CodeBuddy need nothing extra — both read the project's own
`.mcp.json` — so `init` says so rather than writing a second file.

Where the token goes follows what each client can do. `.mcp.json` and
`.cursor/mcp.json` carry it literally — Cursor is a desktop app launched with
no shell environment to read one from — and both are added to the
`.gitignore` block `init` maintains (with the database, its sidecars and
`logs/`). `.trae/mcp.json` is the same case — Trae is a desktop IDE
too — and is gitignored with them. `.opencode.json` and
`.gemini/settings.json` existed before `init` and are as likely as not
committed, so they get an environment reference in the client's own syntax
(`{env:DRSG_TOKEN}`, `$DRSG_TOKEN`) instead, as the Codex entry always has
(`bearer_token_env_var`) and as Kimi Code does by naming the variable
(`bearerTokenEnvVar`); `init` prints the
`export DRSG_TOKEN=$(drsg token --unhide)` line to run before launching those
clients here. That line reads the token out of `.mcp.json` rather than spelling
it, because `init`'s output ends up in terminal scrollback, CI logs and agent
transcripts. `drsg token` is what reads it back, so the line needs nothing else
installed and says what to do when no `init` has run here yet.

**`drsg token` masks what it prints** — `AAAA…LmXw <40 characters>`, enough to
tell one repository's token from another's and not enough to use. `--unhide`
prints the token itself, for the command substitution above, where it goes
straight into the environment without reaching the screen. A token too short
for its ends to give it away is masked whole.

Trae gates project-scoped MCP behind **Settings > Beta > Enable Project MCP**,
so the file `init` writes is ignored until that is on; `init` says so on the
line it prints.

For Claude Code it also installs two hooks, in the repository's
`.claude/settings.local.json` (the per-user file, so nothing lands in the
tree a team shares) with the scripts beside the plugin store. A config tells
an agent where the graph is; the hooks tell it *when* to use it. The
`SessionStart` hook puts the rule in the agent's context — drsg first for
every code question, shell last — where it survives a resume and a
compaction. The `PreToolUse` hook meets an `rg`, `grep`, `cat` or `sed -n`
on code with the verb that answers instead, and `DRSG_RAW=1 <command>` runs
any command untouched, for the file no plane holds. A redirect or heredoc is
a write and passes; a search whose *pattern* holds an arrow (`rg '>'`,
`grep "a -> b"`) is still a search and is redirected. Re-running `init` after
the scripts' directory moved repoints the entries — only entries whose
command is exactly one of drsg's scripts; a team's own hooks, even one named
`my-drsg-shell-guard`, are left alone. The other hosts have no hooks; they get the same rule from the server's own MCP instructions, which
every host places in the system prompt.

```console
$ drsg init
plane 'myrepo' bootstrapped — serve watch pid 48213, http://127.0.0.1:51900/mcp
  + wrote ./.mcp.json
  + Claude Code: hooks in ./.claude/settings.local.json — a shell search or read on code is redirected to the drsg tools (DRSG_RAW=1 <command> runs it anyway)
  + Cursor: wrote ./.cursor/mcp.json
  + Gemini CLI: wrote ./.gemini/settings.json (no token inside it — export DRSG_TOKEN=$(drsg token --unhide) before launching `gemini` here)
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

**`--rebuild` drops the plane and reads the tree again.** The parser plugins
decide what a source file means, so an upgraded plugin only shows up in a
plane that has been folded through the new one — a resumed plane keeps
whatever the old parser made of the files it had already seen. `--rebuild`
stops the server that holds the database (it also holds the old copy of every
plugin), rebuilds from the whole tree, and comes back on the same address and
token, so no agent's configuration changes underneath it:

```console
$ drsg plugin install rust        # a newer parser
$ drsg init --rebuild
stopping serve watch pid 51002 — the plane is about to be rebuilt
plane 'myrepo' rebuilt — serve watch pid 51188, http://127.0.0.1:51900/mcp
```

Facts only, as ever: embeddings come back on the next `drsg digest`.

So: once per repository, then again whenever the server is gone, and with
`--rebuild` when a plugin has changed what the tree means.

## What the facts buy an agent

An agent working from grep reconstructs structure on every question: search,
open files, read, infer who calls what, repeat. A digested plane has already
done that work, once, with a parser — so structural questions become **one
round trip** instead of a search-and-read loop.

Twelve verbs carry the workload, identical over MCP
([Chapter 8](./mcp.md)) and the CLI ([Chapter 7](./embedded-cli.md); `grep`,
`snippet` and `recall` read the tree the plane was parsed from, which `--root`
names when the plane records none of its own):

| Verb | The question it answers |
|---|---|
| `context` | everything about one symbol — definition, callers with call sites, callees, references — the primary verb |
| `search` | "I don't know the name": semantic top-k over the plane's embeddings |
| `describe` | one symbol's properties — the lightweight node-only view |
| `grep` | text over the watched source tree — literal or regex, path-scoped, with context lines; each hit names the symbol it falls in; bounded and counted |
| `trace` | how one symbol reaches another: the shortest recorded call path |
| `impact` | blast radius: everything reaching a symbol, grouped by distance — and, asked `--across`, which other repositories the radius reaches |
| `fathom` | what kind of place a symbol sits in: the region within a few hops, by label and edge type, with its hubs |
| `snippet` | a symbol's source text, or a range of a file (`path:start-end`) — the `sed -n` an agent no longer needs |
| `recall` | the code as it was at a commit — a file, a symbol's declaration then, a search of that tree, or a diff between two revisions — the `git show` and `git log -p` an agent no longer needs |
| `history` | the repository behind the code: where HEAD is, what the branches and tags point at, what was rebased, and the newest commits |
| `deployed` | what commit a running thing was built from, and what runs a commit — the manifests' ops plane joined to the history beside it |
| `wire` | what serves an HTTP path and what asks for it, matched across every language in the plane — and across the planes it attaches |

`grep` and `snippet` open files only inside the tree they belong to — the
directory the server watches, or the one a plane was digested from — and refuse
a path that leaves it (`..`, an absolute path, a symlink out of the checkout);
see [Chapter 8](./mcp.md) for how that differs between the stdio and the shared
server.

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

History is the other half. A digest of a git checkout also writes
`<name>_git`, and `recall` reads the code as it was at any commit that plane
names — by sha, branch, tag, date or `HEAD~n` — so "what did this function look
like before the refactor" is one call rather than a checkout. A symbol is
located by parsing its file as it was at that commit, not by today's line
numbers, and a symbol the commit did not declare is reported as absent, with
what its file did declare.

Operations are the third plane. A digest of Compose files, Kubernetes
manifests or Terraform configuration writes `<name>_ops` — the workloads, the
containers, the images they run, the infrastructure underneath — and
`deployed` joins it to the history through the one string both ends name: the
image's tag. Where a pipeline wrote a revision into that tag, *"which commit
is this service running"* is one call. Where it did not, the answer says which
of the reasons applies — a floating tag, a digest that pins bytes rather than
provenance, a revision no commit here holds — because a verb that answered
all three with nothing would leave a reader unable to tell a gap from a fact.
Planes are hard namespaces, so this is a read of two of them rather than an
edge between them.

## Routes: where the graph stops being one repository

A call edge needs a callee the parser can see. An HTTP request has none: a page
calls `/api/v1/orders/42` and the handler is a controller in another language,
often in another repository, with nothing between them but a string. Such a
graph holds both files and no connection.

Each language states its routes in its own way — an annotation carrying a path
(`@GetMapping`, `@app.get`, `#[get]`), a registration call whose name is the
verb (`app.get('/x', h)`, `r.GET`, `http.HandleFunc`), or a client call with a
path argument. The plugins record all three in one vocabulary, on the symbol
that already exists: **`serves`** for a declaration, **`requests`** for a call,
each a `VERB /path/{}` with every variable segment reduced to `{}`. The Go,
Java, Python, Rust and TypeScript plugins write them today.

Nothing new is drawn in the graph. No `Route` node, no `SERVES` edge — a
synthesised edge would freeze a guess, and a path assembled at runtime is a
guess. `wire` computes the correspondence per call and reports what it could
not match: a request reaching no declaration, a declaration nothing asks for
(a dead endpoint, useful in itself), and a prefix the source never states. A
side that writes no verb matches every verb on the path **and says so**.

`attach` is the second half. Frontend and backend are separate repositories,
so they are separate planes, and an attachment records the one fact that joins
them — *this plane may ask that one*. It carries no vocabulary of its own:
what the question is, and what key the two sides match on, belongs to the verb
asking it — so any verb that needs to cross graphs reads the same record.

```console
$ drsg attach fe --plane be --both     # two planes on this drsg, both ways
$ drsg attach front --plane be --url http://127.0.0.1:36500 --as fe
$ drsg attachments --plane be --probe  # does each one answer right now?
```

A plane on this drsg is read directly. One another drsg serves is read over
`plane.query`, the read method every drsg already serves — so crossing
instances adds no protocol and no SDK method, and an attachment that cannot be
reached is named with the reason rather than skipped. Every row names the graph
it came from, so an answer spanning two repositories still says where each
fact lives.

> **Two senses of "attached".** A plane whose *source tree* is synced is also
> called attached (`serve watch`, `synced_root`). That is the tree behind one
> plane; an attachment here is another plane this one may ask. The two are
> unrelated.

## A shared library, and the repositories that depend on it

"Which business repositories would this change break?" is the same question
`impact` already answers, asked one repository out. A change to a class in a
common library is local; its consequences are not, and a radius walked in one
plane cannot see them.

`impact --across` answers it in the other graphs too, by **two keys both sides
already state** — nothing new is extracted, and no plugin needed changing:

| join | this plane holds | a dependent holds |
|---|---|---|
| **build** | `Manifest.declares` — the coordinate its own manifest publishes | a node keyed by that coordinate, with `version` and `scope` on the `DEPENDS_ON` edge |
| **code** | the changed symbol, and every key in its radius | the same key as a stand-in, labelled `External` |

The coordinate is *read from this plane*, never mapped onto it. A Maven
coordinate (`com.acme:common-base`) and a Java package (`com.acme.common…`)
cannot be mapped to one another without a guess — which is why the `deps`
plugin records a build dependency and declines to join it to any import.
Standing in the library there is nothing to guess: its own `pom.xml` says
which coordinate it publishes, and a dependent's `pom.xml` keys its
declaration the same way.

```console
$ drsg impact com.acme.common.BaseEntity --plane commonlib --across all
com.acme.common.BaseEntity  Class  src/main/java/com/acme/common/BaseEntity.java:3-6
depth 1 (1):
  com.acme.common.Dated  Class  src/main/java/com/acme/common/Dated.java:3-5  [EXTENDS]
total affected within depth 3: 1
note: recorded call edges only — calls the parser could not resolve (dynamic dispatch, untyped receivers) are absent, so this is a lower bound.

across  commonlib, svcorders, svcbilling
in svcorders
  declares  com.acme:common-base  2.4.1  [runtime]  pom.xml
  com.acme.common.Dated  ← 1
    com.acme.orders.Order  Class  src/main/java/com/acme/orders/Order.java:5-8  [EXTENDS]
in svcbilling
  declares  com.acme:common-base  2.3.0  [runtime]  pom.xml
2 graph(s) read, 2 hold(s) something: 2 declare(s) a dependency, 1 reach(es) it in code
1 of the 2 key(s) offered are referenced elsewhere
```

`svcorders` is reached through `Dated` rather than through the changed class
itself — a key the radius holds, not only its seed. `svcbilling` declares the
dependency at an older version and touches none of it, which is an answer
worth having.

Two scopes, because two environments ask this differently. `--across` alone
reads the planes this one attaches: explicit, and the only scope that can
reach another drsg. `--across all` reads every plane this instance holds, so a
machine that was handed thirty repositories needs no setup to be asked across
them; an empty plane and a `…_git` history plane are not among them. Either
way the answer names the graphs it read, and the ones it could not.

Without `--across` the answer is unchanged, byte for byte. Unlike `wire`,
which crosses graphs unasked because a route's two halves live in different
repositories by nature, a radius *is* complete within one repository — adding
graphs changes what the total counts, so a caller says how far to look.

A graph another drsg serves answers with nodes, since that is what
`plane.query` returns and a version is a fact about an edge. It therefore
reports which keys it holds — itself the statement that something in it
references them, a stand-in existing only where a reference minted one — and
names the call that reads the rest:

```console
across  commonlib, far
in far  at http://127.0.0.1:36600
  declares  com.acme:common-base  (the manifest and its version are read there)
  com.acme.common.Dated  ← referenced; what references it is read there
  ask that drsg for the rest: `impact <key> --plane svcorders`
```

One more distinction it keeps: a graph holding a key *without* `External` is
not a dependent but the same code again — vendored, or digested twice — and is
reported as `holds its own` rather than counted as a user.

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
