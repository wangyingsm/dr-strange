# Listing drsg in the MCP directories

Submission material, kept in the repository so the wording and the version
stay with the code rather than in someone's browser tab. Every listing below
is submitted under a maintainer's own account — nothing here submits itself.

## 1. Official MCP Registry

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

**Keep `version` in step with the release.** The registry treats each version
as its own record, so a listing left at an old number advertises an old
binary. `just bump-version` does not touch this file yet; adding it is a
one-line change when the first listing exists and the number starts mattering.

### Why the package is the OCI image

The registry describes how a client *fetches* the server, and its examples are
npm, pypi, oci, nuget and mcpb. drsg ships platform tarballs on GitHub
releases, which is none of those, so the image the release workflow already
publishes to GHCR is the one honest entry today.

That image serves MCP over HTTP; it does not give a client the
repository-local `drsg init` flow, which is how drsg is actually meant to be
used. **npm distribution (`npx @yingge/drsg`) is what makes this listing
match the product**, and it is already on the Q4 plan — redo the `packages`
block as `npm` when it lands, and the entry stops being a compromise.

## 2. Smithery, Glama, mcp.so

Each reads a repository and builds its own page; none needs a file here beyond
what the README already states. What they do reward is a configuration example
a reader can paste, which `README.md` now carries under "Three commands".

Submit at:

- Smithery — https://smithery.ai/new
- Glama — indexes public GitHub repositories with an MCP server; claim the
  page once it appears.
- mcp.so — https://mcp.so/submit

## 3. awesome-mcp-servers

A pull request adding one line to the list. Proposed entry, matching the
list's existing format:

```markdown
- [wangyingsm/dr-strange](https://github.com/wangyingsm/dr-strange) 🦀 🏠 - Code-intelligence graph: digests a repository into a resolved call graph and answers who-calls-what, blast radius, call paths and HTTP route matching in one round trip.
```

The emoji are that list's own legend: 🦀 Rust, 🏠 local service. Check both
against the legend at the top of the list before opening the PR — it changes.

## 4. Domestic directories

魔搭 MCP 广场, 阿里云百炼, 腾讯云 MCP 广场, and the Trae and Qoder markets each
want an account and a form rather than a file. The Chinese description to
paste is the first paragraph of `README.zh.md`; the install line should name
the mirror flag (`--mirror`), since a reader coming from these pages is on the
network that made the flag necessary.
