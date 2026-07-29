# drsg — TypeScript client for dr-strange

A zero-dependency (platform `fetch` only) client for a `drsg serve` JSON-RPC
endpoint. The method surface and its types are **generated from the server's
OpenRPC schema** (`crates/dr-strange-web/openrpc.json`), so they always match
the wire protocol. Runs anywhere `fetch` exists — Bun, Node 18+, Deno, browsers.

## Install

```bash
bun add drsg      # or: npm install drsg
```

## Use

```ts
import { Drsg, DrsgError, DrsgAuthError } from "drsg";

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
each request as `Authorization: Bearer …`. A missing/invalid credential rejects
with `DrsgAuthError` (code `-32001`); other server errors reject with
`DrsgError` carrying a `.code`.

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
