# End-to-end checks

`just e2e` — or either script directly, with `DRSG_BIN` pointing at a built
binary.

Two scripts against the real binary, 85 checks in all.

## `run.sh` — code intelligence

50 checks against the real wasm parsers and a real git repository. It covers
the three code-intelligence features of the 2027-Q1 plan, in the order a
change meets them:

| Section | What it pins |
|---|---|
| cross-layer edges | Spring MVC routes reach their handlers; a Dubbo provider and consumer are matched on one address; a MyBatis mapper method is implemented by the XML statement beside it, which reads a table and its columns; `trace` crosses all three in one answer |
| `affected` | the tests a change reaches, by `CALLS` and at two hops; what `--safe` widens to; `--seeds` as a loop's input; a clean tree; `--vs` as CI reads a committed change; the staleness warning, present when the plane is behind and absent when it is not |
| `--report` | the section headings, the fold, the per-test reason, the honesty note as a block quote; the radius table; the route table; and the empty-output contract, four times over |

## `store.sh` — what the store says about itself

35 checks over HTTP against a real `drsg serve`, for the three reports that
became 2.33.0 (#112, #113, #114). No plugins and no git: it writes nodes and
asks questions.

| Section | What it pins |
|---|---|
| #112 | `plane.indexes` reports an index's width as `dim`; a query that width contradicts is refused with `-32602` naming both widths and the pair; a search on an *undeclared* property is **not** refused, because there the width is unknown; a bare 16-number array under a declared index is promoted and answers its own query; a two-number array under the same index stays a list |
| #113 | a misspelled parameter is refused by name and writes nothing; the five methods that flatten the time-travel params still ignore the unknown, as documented; `digest.write` takes `labels`, `label` + `extra_labels`, and neither, landing each node under the right labels rather than `Entity` |
| #114 | an expired entry is gone from `node.get`, from a scan, and from Cypher; a search for **one** neighbour answers the live entry when the expired one is nearest; rewriting the expired key lands on the same row and makes it findable again |

It needs a token, and that is the server's own rule rather than a workaround:
with no token set, a loopback bind trusts only its same-origin browser UI, so
curl is refused even for a read. The script exports a throwaway one on a port
nobody else holds, and replaces whatever the environment has rather than
reading it.

Each check is a Python expression over the parsed reply, so an assertion says
what it means (`r['error']['code'] == -32602`) rather than grepping a shape.
The server is killed by pid, never by pattern — a `pkill -f drsg` here would
take this repository's own watch server with it.

## Why a script and not a `#[test]`

Everything here is already covered by unit tests at the level where it is
written: `compact.rs` tests the renderers against a hand-built graph, and the
plugins test their own edges in the `extensions` repository. What no unit test
can state is that the whole stack still agrees — this binary, these parser
versions, this fixture, this git history. A plugin release that drops the
`serves` property on a Dubbo provider breaks nothing in either test suite and
everything in the feature.

So the fixture is digested, not mocked, and the assertions read the same text a
person would read.

## What it is not

Not part of `just gate`. The gate mirrors CI, and CI has no plugin store;
`run.sh` needs one, and a few seconds of digesting. Run both before a release,
and `run.sh` again after `drsg plugin install`.

It does not run the CI templates themselves — `ci/github/action.yml` and
`ci/gitlab/drsg-impact.yml` are platform glue, and their own platform runs
them. What it pins is the contract they are built on: the verbs' flags, the
Markdown shape, and above all that there is *no output at all* when there is
nothing to report, which is what lets a template decide a section with
`[ -s file ]` instead of matching prose.

## The fixture

`fixture/` is one repository with two halves, copied to a temporary directory
and committed there.

The Java half is a single request crossing three frameworks:
`OrderController` (`@RestController`) → `OrderServiceImpl` (`@DubboService`) →
`OrderMapper` (`@Mapper`) → `OrderMapper.xml` → `SELECT … FROM orders`. Plus
`Checkout`, a `@DubboReference` consumer holding the interface and never the
implementation, which is the half `wire` has to match across a boundary.

The Rust half is four tests over three functions, arranged so the set is not
trivially the whole file: one test reaches the change directly, one through a
helper, one reaches nothing it touches, and one reaches it *only from inside a
macro*. That last one is invisible to any parser — a macro body is a token
tree, not code — so it is the lower bound `affected` warns about, in a form
that can be asserted. `--safe` is what recovers it.

Two notes for anyone extending the fixture:

- **Write plain calls.** `assert_eq!(total(&[]), 0)` records no edge at all.
  Bind the call to a variable and assert on that, unless you mean to test the
  blind spot.
- **Keep the fixture's own `logs/` ignored.** drsg's daily log defaults to
  `./logs`, and on a dirty-tree run its own log file would be a changed file
  of its own. The script sets `DRSG_LOG_DIR` outside the repository as well.

## When the cross-layer section fails alone

The store holds an older plugin than the catalog pins. The script says so, and
the fix is `drsg plugin install java xml`. This is not hypothetical: the store
on the machine where these checks were written held java 2.2.0 while the
catalog pinned the 2.3.0 release that carries the Dubbo work, so the fixture
was being parsed by a plugin that predated the feature under test.
