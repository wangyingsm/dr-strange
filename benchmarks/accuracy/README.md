# Accuracy benchmark

[BENCHMARKS.md](../../BENCHMARKS.md) measures what the engine costs.
[AGENT-BENCHMARKS.md](../../AGENT-BENCHMARKS.md) measures how many round trips
an agent needs, and says plainly that its numbers are reported rather than
reproducible. This measures whether the answers are **right**, and everything
it needs is in this directory.

```console
$ uv run --no-project --with tomli python benchmarks/accuracy/crosscheck_go.py
$ uv run --no-project --with tomli python benchmarks/accuracy/score.py
$ uv run --no-project --with tomli python benchmarks/accuracy/score.py --tool text-search
```

The text-search baseline needs nothing installed, so the comparison's floor
can always be reproduced; the other tools need their own binaries on `PATH`
(`--tool gitnexus` and the rest).

## Four questions, not one number

A single "accuracy" percentage would hide the distinction that matters most.
The four arms are the four task types the annual plan names: callers, blast
radius, cross-file references, and same-name symbols (which is what the
precision traps are made of).

- **Recall** — of the calls whose target is unambiguous from syntax alone, how
  many does the tool report? A miss is incompleteness: the agent asks who
  calls this and is told less than the source says.
- **Precision** — of the pairs the key marks as traps, how many does the tool
  assert anyway? A false edge is worse than a miss, because it sends an agent
  to rewrite code that was never involved.
- **Impact** — "what would a change to this reach?", the question an agent
  asks before it edits anything. Scored as a set, by walking each tool's own
  edges backwards from a subject. This is where an edge error stops being one
  line in a report: a missing call hides a whole subtree of callers, and a
  false one drags in code that was never involved. A reach is only counted
  against a tool if the key cannot justify it *at all* — including through
  the candidates at an ambiguous site — so resolving `h = upper; h(s)` is
  treated as the extra capability it is, not as an invented edge.
- **Reference** — cross-file type references: a parameter typed by a struct
  three files away, a literal naming a type from another file. Recall only.
  There is no honest denominator for precision here, because what *else*
  counts as a reference — a method to its receiver, a same-file field, a file
  node to a type — is a modelling choice rather than a fact. A tool with no
  reference concept is reported as out of scope, not as a zero.
- **Honesty** — at the sites where syntax genuinely cannot settle the target,
  does the tool say so or pick one and sound certain? Reported in parts: a
  **per-site** gap names the call it could not resolve; a **blanket**
  disclaimer says only that something may be missing. Both beat guessing; only
  the first tells you where. The three metrics this breaks into are defined
  below, under [The three honesty metrics](#the-three-honesty-metrics).

## The corpus, and why it is a fixture

`corpus/<language>/` holds a small program written so that its hard cases are
the ones a real codebase has — an interface with two implementations, a
decorator holding the interface it decorates, a function value, a bound method
value, a closure, a deferred call, a goroutine, and a same-name method on an
unrelated type. Beside it, `key.toml` states which calls are real, which are
ambiguous, and which pairs must never be asserted, each with its reason.

A fixture rather than a real repository because ground truth has to come from
somewhere, and for a real tree it would come from whoever wrote the key.
The honest weakness of a fixture is the mirror image: **the author of the
fixture and the author of the key are the same person**, so a construct neither
thought of is in neither. Two things push against that:

1. The hard cases are taken from the language's own idioms and from what other
   code-graph tools document as their limits — not from what drsg is known to
   handle.
2. `crosscheck_go.py` checks the key against an independent tool.

C's oracle is the object code. `objdump -d -r` distinguishes the two kinds of
call without any approximation at all: a direct call carries a symbol, and a
call through a struct of function pointers compiles to `call *%rcx`, naming
nothing — which is exactly the line the key draws.

Kotlin compiles to JVM bytecode, so `javap` serves it as it serves Java, with
three wrinkles the check accounts for: top-level functions live in a synthetic
`<File>Kt` class, every public function opens with the compiler's own
`Intrinsics.checkNotNullParameter`, and a lambda is reached by
`invokedynamic`.

Java's oracle is the compiler's own output. `javac` turns every call into an
explicit invoke instruction naming its target and `javap -c` prints them, so
the check reads what actually runs. Its bound is the same as TypeScript's —
bytecode records the *static* target, so an interface call is
`invokeinterface acme/Store.get` and never an implementation.

TypeScript has the strongest oracle of all, and it needs no approximation:
`crosscheck_ts.mjs` drives the compiler's own type checker — the same
authority an editor uses for go-to-definition. Its bound is the mirror of
RTA's: the checker answers for the *declared* type, so a call through an
interface resolves to the interface's method and never to an implementation.
That is exactly the line the key draws between `resolved` and `ambiguous`, so
it checks both sides of it.

Rust has none. rust-analyzer emits no call graph, and a MIR-level analysis
would be a larger undertaking than the fixture it would check. `crosscheck_rust.py`
therefore checks the single claim a tool can settle — that the `macro_rules!`
expands to a call to `normalize`, via `cargo expand` — and says plainly that
the rest of that key rests on reading the source. It is the weakest ground
here.

Python has no oracle of that kind — `pycg` does not install under a current
interpreter, and a type checker resolves types without emitting a call graph.
So `crosscheck_py.py` takes the other road: it exercises the fixture under
`sys.setprofile` and records the calls that **actually happen**. That is truth
rather than approximation, and it fails the opposite way: a dynamic trace
cannot over-report, only under-report by not exercising a path. Edges it never
reaches are printed as uncovered rather than counted as agreement, so a gap in
the exercise is visible instead of silent.

## What the cross-check found, including about itself

`callgraph` (golang.org/x/tools) is the obvious oracle for Go, and it is not
usable as the key. Both its algorithms are wrong here, in opposite directions:

- **RTA** is reachability-based. It never sees a function `main` cannot reach,
  so `viaMethodValue`, `viaDefer` and `viaGo` are absent from its answer — not
  denied, absent.
- **CHA** sees every function and resolves a call through a function-typed
  variable to *every function in the universe with that signature*. For this
  fixture it claims `viaFuncValue` calls `strings.ToUpper`, `path.Base` and
  thirty others. None of that is true of the program.

So the cross-check is used as a bound, not as truth: an edge in `resolved`
must appear in at least one algorithm, and an edge in `absent` must appear in
neither RTA nor the key's own reasoning. On its first run it caught a real
defect — the key asserted `viaClosure -> upper`, while callgraph models the
closure as a synthetic `viaClosure$1` and routes the call through it. Those
synthetic nodes now collapse into their parent, which is the right reading for
a key that has no symbol for a closure.

## Results

drsg 2.22.0, with the plugin versions the catalog published on 2026-10-17 —
`go` 2.2.0, `ts` 2.2.0, `rust` 2.2.0, `c` 2.1.0, `kotlin` 1.1.0, `csharp`
1.1.0, `java` 2.2.0, `py` 2.1.0. Six of those are releases this benchmark
caused: see [what it found in drsg](#what-this-found-in-drsg).

| | recall | precision | references | ambiguous sites not guessed | of those, gap **named** | key checked by |
|---|---|---|---|---|---|---|
| Go | **100%** (14/14) | **100%** (5/5) | **7/7** | 6/6 | 2 of 6 | `callgraph` (RTA + CHA) |
| Java | **100%** (7/7) | **100%** (5/5) | **4/4** | 6/6 | 4 of 6 | compiled bytecode |
| Python | **100%** (8/8) | **100%** (5/5) | — none to find | 7/7 | 5 of 7 | a real execution trace |
| TypeScript | **100%** (7/7) | **100%** (5/5) | **1/1** | 7/7 | 0 of 7 | the TypeScript checker |
| C# | **100%** (9/9) | **100%** (5/5) | **1/1** | 6/6 | 4 of 6 | Roslyn, in CI |
| Kotlin | **100%** (9/9) | **100%** (5/5) | **1/1** | 5/5 | 4 of 5 | compiled bytecode |
| C | 86% (6/7) | **100%** (5/5) | **6/6** | 5/5 | 0 of 5, 1 followed | the object code |
| Rust | 83% (5/6) | **100%** (5/5) | **1/1** | 6/6 | 5 of 6 | `cargo expand`, macro only |

Totals: **recall 65/67, precision 40/40 — no false edge anywhere — impact
59/61, references 21/21.** The two misses are the same one twice, and the
section below says why they are still there.

Every key is now checked by something that is not this project. C#'s oracle
runs in CI rather than locally — `.github/workflows/accuracy.yml`, because the
machine these fixtures were written on has no C# compiler — and its first run
found that the fixture **did not compile**: it leaned on a project's
`ImplicitUsings`, so `List<>` and `Func<,>` were unresolved to anything reading
the files directly, and it had already been scored by three tools in that
state. The check reports a compile failure rather than an empty graph, which
is the only reason that surfaced instead of passing as "no disagreements".

**Nothing was guessed, in any language, at any site where syntax cannot settle
the target. No false edge anywhere** — including every same-name trap, which a
tool matching on a bare method name would take.

### Where the plugins differ, which is the point of the last column

Precision is uniform and recall nearly so. What varies is whether a parser
*says* it could not resolve something, and that is a plugin decision rather
than a property of the engine.

- **Rust and Java point at the interface.** A `Box<dyn Store>` call becomes an
  edge to the trait method `Store::get`; a Java interface call becomes one to
  `acme.Store.get`. That names what was called without pretending to know
  which implementation runs — the same answer the TypeScript checker and the
  JVM's own bytecode give.
- **Python names the gap another way**, writing the unresolved expression into
  the key (`?::service.py::self.store.get`).
- **Go and TypeScript record nothing** for the same shape. For Go's
  `s.store.Get(id)` — where `store` is declared `Store` in the same file —
  there is neither an edge to `Store.Get` nor an unresolved reference. An agent
  asking `context Service.Load` is told it has no callees, and learns only from
  the blanket lower-bound note that this may be incomplete. TypeScript behaves
  the same way at all seven of its ambiguous sites.

That asymmetry is fixable rather than inherent. Three of the five plugins
already do what the other two do not, and in every case the parser has already
recorded the interface method as a node — Go's `Store.Get` and TypeScript's
`Store.get` are both in the graph, unconnected to the call that goes through
them.

### The two recall misses, and why they are staying

**A call a macro generates.** Rust's `via_macro` and C's `via_macro` both reach
`normalize` through a macro. `cargo expand` and the object code respectively
confirm the call; the call exists in no line of source a parser reads.

Reaching 67/67 would mean expanding macros inside a parser, and the Rust
plugin declines on stated grounds: `macro_rules` has hygiene and recursion, and
a proc macro is arbitrary code that has to be *run*. So expansion stays the
compiler's job, and both plugins now do the honest thing instead — name the
macro at the call site, so a body that hides a call behind one cannot be
mistaken for a body that calls nothing. C already did; Rust marked
item-position invocations and was **silent** in expression position, which was
a real defect against its own stated principle and is fixed in `rust` 2.2.0.

These two are therefore a deliberate limit, not a to-do. A tool that reported
them would be guessing or running a compiler; this one says where it stopped.

### What this found in drsg

Six plugin releases, all on 2026-10-17, every one of them a gap this benchmark
found and no one had reported:

| fix | release |
|---|---|
| An extension method is dispatched on the receiver's static type — the same miss in two independently written plugins, which pointed at how extension dispatch was modelled rather than at either plugin | `kotlin` 1.1.0, `csharp` 1.1.0 |
| A composite literal names the type it builds: `&Service{store: &AuditStore{…}}` names three types from two other files, and nesting is why only the outermost was seen | `go` 2.2.0 |
| An anonymous `typedef struct { … } Name` owns its fields — the shape most C in the wild declares a struct in, whose fields had no tag to belong to | `c` 2.1.0 |
| A parameter property declares a field of its class: `constructor(private store: Store)`, the shorthand most TypeScript services are written with | `ts` 2.2.0 |
| A macro invoked in a body is marked, not silent | `rust` 2.2.0 |

The extension-method and reference gaps were worth more than the recall points
they bought: each was a shape that appears in ordinary code, not a corner the
fixtures invented.

### One plugin follows a local binding

C resolves `f = normalize; f(value)` to `normalize`. That is not a guess — it
is the only binding in scope, and following it is local dataflow. It is scored
as its own outcome rather than as either a resolution or a guess, because the
other seven plugins leave the same shape unresolved. They are incomplete there,
not dishonest; C is more complete.

## Blast radius, and cross-file references

The two arms added last. Impact is derived from each tool's own edges, so it
is not an independent measurement — it is what the edge errors *cost* when a
question is asked through them.

| | impact | cross-file references |
|---|---|---|
| drsg | 59/61 | **21/21** |
| codegraph | 55/61 | — no reference edges |
| codebase-memory-mcp | 55/61 | 13/21 |

**On impact the three are close, and that is the honest result.** The arm
mostly re-reads the call edges, so it separates tools only where a false edge
compounds: codebase-memory-mcp's one wrong edge in the Go fixture
(`Cache.Get -> AuditStore.note`) puts **five** symbols in the blast radius of
`AuditStore.note` that never touch it — `Cache.Get`, `Service.Warm`,
`viaDefer`, `viaGo`, `viaMethodValue`. An agent asking what a change to
`note` affects is handed five files of unrelated work. That is the cost of a
false edge stated in the units an agent actually feels, and it is why
precision is the number to read first.

**On references, drsg now finds every one. It did not when the arm was
written** — the table below is the comparison that caused six plugin
releases, and the rows are what the first run said:

| | declared | drsg, first run | drsg, after the fixes | codebase-memory-mcp |
|---|---|---|---|---|
| Go | 7 | 4 | **7** | 6 |
| C | 6 | 4 | **6** | 4 |
| Java | 4 | 4 | **4** | 3 |
| C#, Kotlin, Rust | 1 each | 1 each | **1** each | 0 each |
| TypeScript | 1 | 0 | **1** | 0 |
| Python | 0 | — | — | — |

Losing the Go row to codebase-memory-mcp, 4 against 6, is what made the arm
worth having. A benchmark its own author's tool wins everywhere teaches
nothing.

Two notes on what this arm is worth. **It is thin outside Go, Java and C** —
the other fixtures declare one reference each, because single-purpose files
with few types is what makes the call arm readable. And **Python declares
none**: its `Service.__init__(self, store)` carries no annotation, so there
is no cross-file type reference in that fixture to find. An empty arm there
is a fact about the code, not a tool failing to see something.

`IMPLEMENTS` counts as a reference for both tools — a class naming the
interface it implements is a reference to a type declared elsewhere,
whatever edge kind it lands on. Without that, Java's `MemStore -> Store`
read as a miss for drsg when it was recorded all along.

## Against the other tools, and against no tool at all

The point of a tool-neutral key. Same fixture, same key, same questions,
scored by the same code — `--tool codegraph`, `--tool codebase-memory-mcp`,
`--tool gitnexus` or `--tool text-search`.

drsg 2.27.0 with the catalog plugins of 2026-11-08, codegraph 1.6.2,
codebase-memory-mcp 0.12.1, GitNexus 1.6.12, every fixture. The second column
is drsg as this benchmark first scored it, before the six releases it caused —
kept because a comparison that quietly improves one side is not one:

Re-scored at 2.27.0 on 2026-11-08, five releases after the figures were first
published at 2.22.0: every cell reproduced, drsg's and GitNexus's alike. The
interval includes a fix to which of two declarations outranks the other, so
the graph those releases resolve is not the graph 2.22.0 resolved — and no
scored answer moved. The two columns this run could not refresh say their own
version: codegraph is not installed here, and the `codebase-memory-mcp` on
this machine is older than the 0.12.1 measured, so replacing its figures
would have made the comparison worse rather than newer.

| | drsg, first run | drsg now | codegraph | codebase-memory-mcp | GitNexus | text search |
|---|---|---|---|---|---|---|
| C | 6/7 | 6/7 | 6/7 | **7/7** | 6/7 | 6/7 |
| C# | 8/9 | **9/9** | 8/9 | **9/9**, 2 false | 8/9 | **9/9**, 17 false |
| Go | **14/14** | **14/14** | 11/14 | 10/14, 2 false | 10/14 | **14/14**, 31 false |
| Java | **7/7** | **7/7** | 5/7 | 5/7 | 5/7 | **7/7**, 17 false |
| Kotlin | 8/9 | **9/9** | **9/9** | **9/9**, 2 false | 4/9 | **9/9**, 17 false |
| Python | **8/8** | **8/8** | 7/8 | 7/8 | 7/8 | **8/8**, 11 false |
| Rust | 5/6 | 5/6 | 5/6 | 4/6, 2 false | 3/6, 1 false | 5/6, 19 false |
| TypeScript | **7/7** | **7/7** | 6/7 | 6/7 | 6/7 | **7/7**, 21 false |
| **total recall** | 63/67 | **65/67** | 57/67 | 57/67 | 49/67 | **65/67** |
| **false edges** | **0** | **0** | **0** | 8 | 1 | 133 |

**Read the last two columns together, because they are the finding.** A plain
text search matches drsg's recall exactly — 65 of 67 — and asserts 133 calls
that do not exist. So the recall column, taken alone, does not separate a
resolved call graph from `rg`. Everything that distinguishes them is in the
row beneath it, and in the honesty arm.

**codebase-memory-mcp asserts eight calls that do not exist**, and every one is
the same mistake: it picks a method by name without regard to the declared
type of what it was called on. From its own graph, for the Kotlin fixture:

```
MATCH (a)-[:CALLS]->(b) WHERE a.name = 'load'
  load  src/Service.kt:14  ->  get  src/Service.kt:7
```

Line 14 is `fun load(key: String): String? = store.get(key)`, where `store` is
typed `Store`. Line 7 is `fun get(key: Int): String? = hits[key]` — `Cache.get`,
which `load` never touches. It chose the `get` in the same file over the one
the declared type names, and it does this in C#, Go, Kotlin and Rust. In Go it
also has `Cache.Get` calling `AuditStore.note`, where `Cache.Get`'s entire body
is `return c.hits[key]`.

That is the failure this fixture was built to expose, and the one that costs
most: an agent told `Cache.Get` reaches `AuditStore.note` goes and reads code
that was never involved.

**codegraph records no calls into the standard library** — `callees normalize`
returns zero where the source calls `strings.TrimSpace`. That is a scope
decision rather than a resolution failure, and it accounts for three of its ten
misses. It asserts no false edge anywhere, and it is the only tool that
resolves both local function bindings in the Go fixture.

**GitNexus is the most conservative tool here**, and it is a coherent position
rather than a weakness: it avoids all forty traps, invents one edge in eight
fixtures, and pays for that with the lowest recall — 49 of 67. It draws a
`CALLS` edge only where its Tree-sitter pass is confident, and it puts that
confidence *on the edge* (`confidence: 0.8, reason: "callable-value-flow"`),
which no other tool here does. This harness does not filter on it: picking a
threshold would be the benchmark deciding what the tool meant. Its weak
fixtures are Kotlin (4/9) and Rust (3/6) — both languages where the calls it
misses are on `self` inside the same `impl` or class.

Its one unaccounted edge is `Store::put -> AuditStore::note`, a trait method to
a private helper of one implementation — the same shape of mistake that costs
codebase-memory-mcp eight edges, made once.

**And a plain text search is the baseline that gives the rest their meaning.**
It has no index, no parser and no concept of a type: for every symbol it
searches the corpus for the bare name on a word boundary and attributes each
hit to the declaration it falls inside. It is given the strongest honest form
of that — a declaration is not a use of itself, and a comment-only line is
discarded, since attributing a doc comment to the declaration above it would
be an artefact of the attribution rather than anything a reader would
conclude. What it cannot be given is language awareness, and that is the
measurement:

- It finds **65 of 67** real calls, tying the best tool here. Text search is
  very good at recall, because a call usually does mention the name it calls.
- It asserts **23 of the 40 traps** and **110 further edges** between known
  symbols. In Go it has `Cache.Get` calling `Store.Get`, `MemStore.Get` and
  `AuditStore.Get`, because all four are spelled `Get`.
- It **names no gap at all** — 0 per-site, 0 blanket, in every fixture. It
  cannot: a text search has no notion of a call it failed to resolve.
- Its blast radius is unusable for the same reason. In Go it reports every
  reaching symbol correctly and 25 more that do not reach at all.

The C fixture is the exception that proves the mechanism: C has no methods, so
nothing shares a bare name, and there the baseline asserts **no** false edge.
Every false edge in the other seven comes from a name collision that only a
resolved type can settle.

### The three honesty metrics

The annual plan names three, and they are not the same question. The first two
are rates over this corpus, printed by `score.py` for every tool. The third is
a property of a tool's own reach answer rather than of any fixture, so it is
recorded here with the evidence.

**1. Unresolved-reference disclosure rate** — of the ambiguous sites where a
tool drew no edge, how many did it *name*? A per-site gap ("a name was called
here and I could not say what") sends an agent to the right line. A blanket
"this is a lower bound" tells it only that something, somewhere, is missing.

**2. Ambiguity misjudgement rate** — of the sites where a guess was available,
how many did the tool pick a candidate at and sound certain? Sites with a
single binding in scope are excluded from the denominator: following
`f = normalize; f(x)` is resolution, not a guess, and the scorer credits it
separately as FOLLOWED.

**3. Traversal-boundary disclosure** — when a tool answers "what does a change
to this reach?", does the answer state the bounds of the walk it did? Three
things are observable, and each is either in the answer or not: the **depth**
it reached, whether the set is **exact or a lower bound**, and the **edge
kinds** it followed.

| Tool | Depth | Exact / lower bound | Edge kinds | Probed with |
|---|---|---|---|---|
| drsg | ✓ `depth 1 (1)`, `total affected within depth 3: 2` | ✓ *"recorded call edges only — calls the parser could not resolve … so this is a lower bound"* | ✓ `[CALLS]` on every hop | `drsg impact normalize` |
| GitNexus | ✓ `byDepthCounts {"1":1,"2":1}` | ✓ `"epistemic": "exact"` | ✓ `relationType` per entry, with a `confidence` | `gitnexus impact normalize` |
| codegraph | n/a | n/a | n/a | `callers` / `callees` answer one hop, so there is no traversal to bound |
| codebase-memory-mcp | ✗ | ✗ | ✗ | the walk is whatever Cypher the caller writes, so the tool states nothing about it |
| text search | n/a | n/a | n/a | no notion of reach at all |

Both graph engines that *have* a reach verb disclose all three, and they do it
differently in a way worth copying in each direction: GitNexus puts an
`epistemic` field on the answer and a confidence on every edge, where drsg's
note says *why* the set is a lower bound — the unresolved dispatch that caused
it. Neither says what the other says.

The row that matters is the fourth. A tool whose only reach interface is a
query language hands the traversal to the caller, which means nothing in the
answer says how deep it went or what it could not follow — and an agent
writing `MATCH (a)-[:CALLS*]->(b)` has no way to learn that from the result.

Over all eight fixtures, with the tools this machine could run:

| Tool | Disclosure rate | Misjudgement rate |
|---|---|---|
| drsg | **24/47 — 51%** named rather than left to the blanket note | **0/47 — 0%** |
| GitNexus | 2/23 — 9% | 16/39 — 41% |
| text search | 0/11 — 0% | 28/39 — 72% |

The two rates pull in opposite directions, which is why both are needed. A
tool can score 0% misjudgement by resolving nothing and disclosing nothing;
text search scores 0% disclosure for the opposite reason — it leaves almost
no site unresolved, because it guesses at nearly all of them. GitNexus sits
between: it declines to assert a trap anywhere, yet at an ambiguous site it
picks a candidate 41% of the time without saying it did.

Two tools are missing from this table rather than scoring zero in it.
codegraph is not installed on the machine that ran this pass, and the
codebase-memory-mcp build here is 0.9.0 — older than the 0.12.1 the recall
table was measured with, and its CLI no longer takes the `--quiet` flag the
adapter passes. Scoring 0.9.0's honesty beside 0.12.1's recall would put two
different programs in one row.

### Identity is by file and line, never by name

The three tools do not agree on what a symbol is called. drsg writes
`acme/orders.Cache.Get`; codegraph writes `Get` and distinguishes it from the
other two by printing `service.go:30`; codebase-memory-mcp qualifies an
interface method but leaves a struct's method bare. So every key records where
each symbol is declared, and each adapter maps whatever a tool returns back
through that. Matching on names would have measured this harness's
name-mangling and called it a competitor's accuracy.

An interface method to its own implementations is also **not** counted as a
false call: codegraph answers `callees Store.Get` with `MemStore.Get` and
`AuditStore.Get`, which is a defensible model of what an agent wants to know.

### The harness was wrong before it was right

Eleven times, the first result slandered a tool and the raw output disagreed.
Worth stating, because a comparative benchmark that has never been wrong has
probably not been checked. Nine of the eleven made a *competitor* look worse;
the last two, added with the reference arm, made **drsg** look worse — the
bug does not care which way it points:

1. codegraph prints a **subject's** location inline and a **callee's** on the
   next line; parsing only the second made every answer look off-subject, and
   `main` appeared to call nothing.
2. Querying the bare `Get` instead of `AuditStore.Get` answered about a
   different method, and its callees were attributed to the wrong symbol.
3. Retrying a legitimate "0 callees" with the bare name invented **nine**
   false edges for a tool that had asserted none.
4. A query matching several definitions prints **one block per definition**;
   folding them under the first subject invented eight more.
5. Externals have no line in the fixture, so three real stdlib edges were
   dropped as missing before name-matching was added for them.
6. Matching a path by basename worked only for the flat Go fixture and scored
   every nested corpus at **zero** — four languages reported as finding
   nothing at all.
7. `codegraph init` was run with its exit status ignored, so a failed index
   scored as a hopeless tool rather than a broken harness. It is fatal now.
8. Reusing one working directory let a tool's background daemon race the
   copy, and a key named `service.Service.save` was passed to a tool that
   wanted `Service.save` — TypeScript scored 14% on a fixture it indexes
   perfectly.
9. Deriving the other tool's project name by munging a path, instead of
   reading the name its own indexer returned, queried a project that did not
   exist and reported 0% for two more languages.
10. The reference arm resolved both ends of an edge through `[symbols]`,
   which holds only functions and methods. Every reference *points at a
   type*, so nothing resolved and codebase-memory-mcp scored 0 of 7 on a
   fixture where its own output plainly contained `main -> Service`. Types
   now live in a separate `[types]` table, kept out of the call universe so
   that a constructor edge does not become a false call.
11. The `[types]` entries were then written as bare names — `Service`, `Store`
   — while each key names symbols its own way (`service::Service` in Rust,
   `acme.Service` in Java). drsg scored **0% on seven of eight languages**
   for references it records perfectly. The C key had a second form of the
   same bug: its `strip` pattern covered `.c::` but not `.h::`, so a type
   declared in a header could never match. This is bug 8 again, which is the
   argument for the `[symbols]` table existing at all.

Every number above was checked against the tool's own raw output before being
written down. The rule that caught all eleven: **when a tool looks bad, read
what it actually printed** — and that rule has to apply to the tool this
benchmark is written for, or it is not a rule. Both new bugs were caught by
disbelieving a flattering-looking 0% for a rival and an unflattering 0% for
drsg, in the same way.

### Not yet covered

- **Rust's key rests mostly on reading the source.** `cargo expand` settles
  the macro entry; nothing independent checks the rest, because no call-graph
  oracle for Rust exists that is smaller than the fixture it would check. It
  is the one key here without a real second opinion.
- **The fixtures are small.** Eight programs of a few dozen lines, chosen for
  the shapes they contain rather than for scale. A tool that does well here
  may do worse on a million lines, and nothing here measures that.
- **Two edge kinds, not all of them.** Calls and cross-file type references
  are scored. `IMPORTS`, `EXTENDS` and route matching are not.
- **codegraph is not compared on references**, because it exposes nothing
  reference-shaped; that arm scores the other four. GitNexus is scored on it
  through the edges that land on a declared type, which is its own vocabulary
  rather than a `REFERENCES` kind, and it reaches only Go and Java — 0/1 in
  four languages where the key records a single reference each.
- **Python has no reference entries**, because its fixture imports a function
  rather than naming a type across files. The arm reports 0/0 there, which is
  no score at all rather than a bad one.
