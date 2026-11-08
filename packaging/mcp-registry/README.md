# Listing drsg in the MCP directories

Submission material, kept in the repository so the wording and the version
stay with the code rather than in someone's browser tab. Every listing below
is submitted under a maintainer's own account — nothing here submits itself.

Status of each directory was checked on 2026-11-08. A directory's intake
changes without notice, so the status line says how it was established:
**verified** means a request to that URL answered, **reported** means a
secondary source said so and the primary page does not state it.

## Order, and why it is not arbitrary

Publish to the **official MCP Registry** before anything else. The other
directories read from it — Glama, PulseMCP and mcp.directory are all described
as importing registry records — so one correct record there is worth more than
four forms filled by hand, and a wrong one propagates just as widely.

Then the directories that index a GitHub repository on their own (Glama,
mcp.so). Then the hand-submitted lists. The Chinese directories last, because
they ask for a package name rather than a repository, and that name comes from
the npm publication the first step also depends on.

## 0. What every listing needs to be true first

- **`server.json` names the current version.** The registry treats each
  version as its own record, so a listing left at an old number advertises an
  old binary. `just bump-version` sets it with the rest — both the record's
  version and the npm package's — so this is true by default rather than by
  anyone remembering.
- **The install line a reader will paste actually works.** Every directory
  below reproduces it, and a reader arriving from one of these pages has no
  other source for it.

## 1. Official MCP Registry — do this one first

**Status: verified** — `registry.modelcontextprotocol.io` answers.

`server.json` is the submission, and it validates against the registry's
published schema (`2025-12-11`):

```console
$ uv run --no-project --with jsonschema python -c "
import json, jsonschema, urllib.request
schema = json.load(urllib.request.urlopen(json.load(open('server.json'))['\$schema']))
jsonschema.validate(json.load(open('server.json')), schema)
print('ok')"
```

Publish with the registry's own CLI, which proves namespace ownership through
GitHub OIDC — the `io.github.wangyingsm/*` namespace is claimable only by that
account:

```console
$ mcp-publisher login github
$ mcp-publisher publish
```

### What the package block declares, and why

The entry is the npm wrapper — `@yingge/drsg`, the install path a client
already has. It declared the OCI image until 2.27.0, because the registry
fetches through npm, pypi, oci, nuget or mcpb and drsg then shipped only
platform tarballs, which are none of those. The npm publication ended that
compromise, and the Chinese directories in §5 ask for exactly this name.
`drsg-embedded` on PyPI is the Python binding rather than the server, and is
not the name to give them.

Three decisions in the file are worth knowing before anyone edits it:

- **It runs `serve`, not `init`.** `init` is the shorter path for a developer
  — it digests, starts the server and writes `.mcp.json` — but it chooses a
  free port and a random token, and a static record cannot name either. So
  the entry runs `serve --addr 127.0.0.1:7700`, stating the address rather
  than defaulting to it, and the transport URL is that same address.
- **It presumes the graph exists.** `serve` opens `./graph.drsg`; `drsg init`
  or `drsg digest` is what creates it. A client that connects before anything
  was digested reaches an empty plane rather than an error.
- **`DRSG_TOKEN` is required, not optional.** `/mcp` is gated at write level,
  and a tokenless server grants only its own same-origin browser UI — an MCP
  client sends no `Origin`, so every call is refused. The header is declared
  as `Bearer {DRSG_TOKEN}`, which the registry substitutes from the
  environment variable, so a client asks for the secret once and it reaches
  both places.

The version is the one field a release has to move, and `just bump-version`
moves it — see §0.

### The package has to claim the server name back

Ownership of an npm package is proved by an **`mcpName` in its
`package.json`** that matches `name` above — `io.github.wangyingsm/dr-strange`
— and the registry reads it from the published package, not from this
repository. `@yingge/drsg` 2.27.0 was published without it, so a publish of
that version is refused however the login went. `packaging/npm/build.py`
writes the field now, which makes the **next** release the first publishable
one; the version in this file and the version that carries `mcpName` have to
be the same release.

## 2. Glama

**Status: verified** — `glama.ai/mcp/servers` answers.

Glama indexes public GitHub repositories that contain an MCP server on its
own, so the listing is likely to exist before anyone submits it. The action is
to **claim** it, which marks it verified. Glama health-checks what it lists;
drsg needs no credential to answer, so nothing has to be provisioned for it.

## 3. mcp.so

**Status: verified** — the form at https://mcp.so/submit was opened on
2026-11-08.

**Listing is paid only: $39, one time.** The submission card is pre-selected
with no free-with-review alternative beside it; "publish immediately without
review" is one of the paid benefits rather than a faster lane past a free
queue. The form itself asks for only a repository URL and a name.

What the fee buys is stated on the page, and it is worth reading literally: a
verified badge, featured placement, and a **dofollow** link from a DR 74
domain with 144K backlinks. That is an SEO placement, which is a different
good from the registry record in §1 — one makes drsg findable by a search
engine, the other makes it resolvable by an MCP client. Neither substitutes
for the other, and only the second is a precondition for anything else here.

## 4. Lists and catalogs submitted by hand

### awesome-mcp-servers

**Status: verified** — its `CONTRIBUTING.md` says contributions are welcome.

A pull request adding one line. The list is explicitly for servers with a
public repository that a reader installs and runs themselves, which is what
drsg is; a remote-only server is directed to `awesome-remote-mcp-servers`
instead. Entries are alphabetical within a category.

```markdown
- [wangyingsm/dr-strange](https://github.com/wangyingsm/dr-strange) 🦀 🏠 - Code-intelligence graph: digests a repository into a resolved call graph and answers who-calls-what, blast radius, call paths and HTTP route matching in one round trip.
```

The emoji are that list's own legend: 🦀 Rust, 🏠 local service. Check both
against the legend at the top of the list before opening the PR — it changes.
Expect the PR to sit: the repository carries thousands of open ones, so this
is a submission to make and forget rather than one to follow up.

### Docker MCP Catalog

**Status: verified** — `hub.docker.com/mcp` answers and `docker/mcp-registry`
exists.

Entry is a pull request to `docker/mcp-registry` rather than a form. The
release workflow already publishes an image to GHCR, so what this listing
needs is the image, not a new build — read that repository's contribution
rules for whether it requires the image on Docker Hub specifically, which is
the one condition GHCR would not satisfy.

## 5. Chinese directories

### 魔搭 ModelScope MCP 广场

**Status: verified** — `modelscope.cn/brand/view/MCP` answers.
**Intake: reported** — a form, with self-service submission still being built.

Submission is a form that the platform processes on the submitter's behalf,
reported as within three working days, and it asks for:

| Field | What drsg gives it |
|---|---|
| English service name | `@yingge/drsg` — the npm name, which is why §1 comes first |
| MCP server config JSON | The `.mcp.json` block `drsg init` writes |
| English README | `README.md` |
| GitHub repository | Optional but worth giving |
| Chinese name, developer, logo, category | Optional |

The Chinese description to paste is the first paragraph of `README.zh.md`, and
`modelscope-readme.zh.md` in this directory is the long-form version written
for this listing. The install line should name the mirror flag (`--mirror`),
since a reader arriving from these pages is on the network that made the flag
necessary.

### 阿里云百炼

**Status: reported** — custom MCP services are created in its own console
(MCP 广场 → MCP 管理 → create and deploy), which hosts a service on their
platform rather than listing one that runs elsewhere. Confirm against the
console, since this is a different mechanism from a directory listing.

### 腾讯云, Trae, Qoder markets

**Status: not established.** No third-party intake process was found for
these. Each wants an account and a form if it has one at all; check their own
documentation rather than trusting a list.

## 6. Directories that cannot be submitted to right now

- **Smithery.** Its MCP Proxy and managed identity **shut down on 2026-10-31**
  — server URLs on `run.tools` and `server.smithery.ai`, the API, Connect,
  Toolbox, accounts, API keys, CLI and SDKs all stopped. The public directory
  remains. `smithery.ai/new`, which an earlier version of this file gave as
  the submission URL, **returns 404**, and the shutdown notice documents no
  replacement submission path; it gives `ani [at] smithery [dot] ai` for
  migration questions. Nothing in drsg pointed at a Smithery URL, so there is
  nothing here to migrate — this entry exists so the dead URL is not tried
  again.
- **PulseMCP.** Submissions are **reported closed**, with records imported
  from the official registry instead. Its submit page refuses automated
  requests, so neither the closure nor a reopening could be verified here.
  Publishing §1 is the whole action: the import is the listing.
- **mcpservers.org, mcp.directory.** Reported to take a form or a registry
  import. Both refuse automated requests, so their status is unverified.
