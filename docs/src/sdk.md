# SDK

Dr Strange ships client SDKs in **five languages** — TypeScript, Python, Go,
Java, and C. Each talks to a running `drsg serve` over JSON-RPC 2.0, and the
typed method surface is **generated from the server's OpenRPC schema**, so every
SDK always matches the wire protocol exactly.

## The same shape everywhere

Construct a client with a base URL and a token (defaulting to `$DRSG_TOKEN`),
then call methods that mirror the server one-to-one:

```typescript
import { Drsg } from "drsg";

const db = new Drsg({ baseUrl: "http://127.0.0.1:7700", token: "…" });
await db.nodeCreate({ plane: "social", key: "ada", labels: ["Person"] });
console.log(await db.dbStats());
```

## Live change feed

Every SDK can open a long-lived WebSocket and subscribe to a plane's change
feed, receiving each committed mutation as it lands — TypeScript via a callback
(auto-reconnecting), Python as a blocking generator, Go as a channel, Java via a
listener, and C via a callback.

## Sections (draft)

- Installing / vendoring each SDK
- Connecting: base URL, token, and the auth model
- Reads and writes (the generated method surface)
- Error handling (`DrsgError` / auth errors, `-32001`)
- The change feed: `watch` in each language
- Codegen: how the SDKs stay in lockstep with OpenRPC
- Language-by-language notes and idioms
