# End-to-end checks

`just e2e` — or `tests/e2e/run.sh` with `DRSG_BIN` pointing at a built binary.

One script, 44 checks, against the real binary, the real wasm parsers and a
real git repository. It covers the three code-intelligence features of the
2027-Q1 plan, in the order a change meets them:

| Section | What it pins |
|---|---|
| cross-layer edges | Spring MVC routes reach their handlers; a Dubbo provider and consumer are matched on one address; a MyBatis mapper method is implemented by the XML statement beside it, which reads a table and its columns; `trace` crosses all three in one answer |
| `affected` | the tests a change reaches, by `CALLS` and at two hops; what `--safe` widens to; `--seeds` as a loop's input; a clean tree; `--vs` as CI reads a committed change; the staleness warning, present when the plane is behind and absent when it is not |
| `--report` | the section headings, the fold, the per-test reason, the honesty note as a block quote; the radius table; the route table; and the empty-output contract, four times over |

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

Not part of `just gate`. The gate mirrors CI, and CI has no plugin store; this
needs one, and a few seconds of digesting. Run it before a release, and after
`drsg plugin install`.

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
