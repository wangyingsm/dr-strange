# drsg — TypeScript client for dr-strange

A zero-dependency (platform `fetch` only) client for a `drsg serve` JSON-RPC
endpoint. The method surface and its types are **generated from the server's
OpenRPC schema** (`crates/dr-strange-web/openrpc.json`), so they always match
the wire protocol. Runs anywhere `fetch` exists — Bun, Node 18+, Deno, browsers.

## Install

The package is not on npm yet; depend on the directory from a checkout of this
repository. Build `dist/` once, then add the directory as a path dependency:

```bash
(cd ../dr-strange/sdk/typescript && bun install && bun run build)
bun add ../dr-strange/sdk/typescript          # or: npm install ../dr-strange/sdk/typescript
```

or, in `package.json`, `"drsg": "file:../dr-strange/sdk/typescript"`. Inside
this repository the examples import the source directly (`../src/index.ts`).

## Use

```ts
import { Drsg, DrsgError, DrsgAuthError, DrsgTimeoutError } from "drsg";

// token defaults to $DRSG_TOKEN; baseUrl defaults to http://127.0.0.1:7700
const db = new Drsg({ baseUrl: "http://127.0.0.1:7700", token: "…" });

await db.nodeCreate({ plane: "startup", key: "alice", labels: ["Person"] });
await db.nodeCreate({ plane: "startup", key: "bob", labels: ["Person"] });
await db.edgeCreate({ plane: "startup", src: "alice", dst: "bob", type: "KNOWS" });

await db.nodeUpdate({ plane: "startup", key: "alice", set: { age: 41 } });
const alice = await db.nodeGet({ plane: "startup", key: "alice" }); // NodeRecord | null
console.log(await db.dbStats());
```

Every method name is the RPC method camelCased (`node.create` → `nodeCreate`,
`plane.set_props` → `planeSetProps`); it takes a single params object keyed by
the schema's wire field names, and returns the method's typed result.

A runnable version is [`examples/quickstart.ts`](examples/quickstart.ts) — `bun examples/quickstart.ts`.

### Auth

The whole surface is authenticated. Pass `token` or set `DRSG_TOKEN`; it rides
each request as `Authorization: Bearer …` — the WebSocket upgrade behind
`watch()` included, under Bun and Node (>= 22), whose `WebSocket` takes a
`headers` init. The standard constructor cannot set headers, so in a browser
window or worker, and under Deno (whose `WebSocket` accepts no headers), the
token rides the URL as `?token=` instead (`WatchOptions.tokenInQuery` forces
either form; pass `false` with the `ws` package). A missing/invalid credential rejects
with `DrsgAuthError` (code `-32001`); other server errors reject with
`DrsgError` carrying a `.code`.

A request that outlives `timeoutMs` (default 30 s) rejects with
`DrsgTimeoutError`, a `DrsgError` (code `-32000`) distinct from the
`connection failed: …` a refused or dropped connection produces.

### Platform types

The declarations reference no ambient `fetch`/`WebSocket` type: `DrsgOptions.fetch`
is a structural `FetchLike` and `WatchOptions.WebSocket` a `WebSocketConstructor`,
which the platform globals of Bun, Node and browsers (and the `ws` package)
satisfy — so the package type-checks in a project without `lib: DOM` or
`@types/bun`.


## Discover

`db.rpcDiscover()` returns the server's live OpenRPC document.

## Develop

The client is generated. After editing the schema:

```bash
cd sdk/typescript
bun run codegen       # regenerate src/generated.ts
bun test              # spins up a real drsg serve (needs the built binary)
bun run typecheck     # tsc --noEmit
```

`test/generated.test.ts` fails if the committed client has drifted from the
schema. The e2e suite skips (does not fail) if no `drsg` binary is found; point
it at one with `$DRSG_BIN`, or build with `cargo build -p dr-strange-cli`.
