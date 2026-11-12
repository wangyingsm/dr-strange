# The pull-request impact report

What a diff does not show: which tests reach the change, what calls the
declarations it touched, and which HTTP routes sit above them. Both templates
digest the checkout into a code graph and ask the same three verbs for it.

A digest is seconds, not minutes — 111k lines of this repository take 2.9s on
one core — so neither template caches a graph. The graph is built at the head
commit of every run, which is also why the answer can never be stale. Only the
parser plugins are cached, because they are downloaded rather than computed.

No model is involved and no API key is needed: the parsers are sandboxed wasm
components, and `--code-only` leaves the documents beside the code out.

## GitHub Actions

```yaml
name: impact
on: pull_request

permissions:
  contents: read
  pull-requests: write          # drop this and the report is the job summary only

jobs:
  impact:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: wangyingsm/dr-strange/ci/github@v2.30.0
        with:
          plugins: rust         # only the languages this repository holds
```

| input | default | what it does |
|---|---|---|
| `plane` | `ci` | the plane the checkout is digested into; only this job reads it |
| `plugins` | the eight languages | space-separated parser plugins; naming only yours is faster on a cold cache |
| `base` | the pull request's base commit | the revision the change is measured from — required on a `push` event |
| `depth` | `3` | hops of incoming edges walked from each changed declaration (1..=6) |
| `safe` | `false` | add every test under the packages the change touched, reached or not |
| `max-symbols` | `10` | how many changed declarations get a radius and a route section of their own; the test set is always complete |
| `comment` | `true` | keep one pull-request comment up to date, editing it on every push |
| `token` | `github.token` | needs `pull-requests: write` for that comment |
| `version` | `latest` | the drsg release to install |

Outputs: `report` (the Markdown file's path, empty when the change named no
declaration) and `tests` (how many tests it reaches), so a later step can gate
on the number or run exactly that set.

A pull request from a fork gets a read-only token. The comment step says so
and leaves the job green; the report is in the job summary either way.

## GitLab CI

```yaml
include:
  - remote: 'https://raw.githubusercontent.com/wangyingsm/dr-strange/v2.30.0/ci/gitlab/drsg-impact.yml'

drsg-impact:
  variables:
    DRSG_PLUGINS: go py
```

The variables are the inputs above, as `DRSG_PLANE`, `DRSG_PLUGINS`,
`DRSG_DEPTH`, `DRSG_SAFE`, `DRSG_MAX_SYMBOLS` and `DRSG_VERSION`. The job
reads `CI_MERGE_REQUEST_DIFF_BASE_SHA` for the base, sets `GIT_DEPTH: 0`
because the report compares two revisions, and writes `drsg-report.md` as an
artifact.

To have it post a note on the merge request, add a project access token with
the `api` scope as a `DRSG_MR_TOKEN` CI variable — `CI_JOB_TOKEN` cannot write
notes. Without one the job says so and keeps the artifact.

## Running it yourself

The templates are shell, not magic. The same report, locally:

```sh
drsg digest . --apply --code-only --no-embed --no-git --plane ci
drsg affected --plane ci --vs origin/main --report
drsg affected --plane ci --vs origin/main --seeds |
  while read -r sym; do drsg impact "$sym" --plane ci --report; done
```

`--report` renders a verb's answer as Markdown, and prints **nothing at all**
when there is nothing to report — a symbol nothing reaches, a declaration on
no route, a change that named no declaration. That is what lets a template
decide whether a section belongs by testing a file for emptiness, rather than
by matching prose that may be reworded.

`--seeds` prints the keys of the declarations the change touched, one per line
and nothing else, which is what the loops above read.

## What the report does not claim

Every section states its own bounds, and the bound is always the same one: a
recorded edge is a lower bound on what calls what. A test that reaches the
change only through dynamic dispatch has no edge to be found by, so it is
missing from the set. `safe` is the answer where a miss costs more than the
time — it adds every test under the packages the change touched, reached or
not.

A plugin the job did not install is a language the graph does not hold, and a
file nothing parsed contributes no declaration. The `affected` section says
how many of the changed files it could place.
