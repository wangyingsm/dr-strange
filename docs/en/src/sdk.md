# SDK

Dr Strange ships client libraries for **five languages** — TypeScript, Python,
Go, Java, and C. Each communicates with a running `drsg serve` over JSON-RPC 2.0,
and its typed method surface is **generated from the server's OpenRPC schema**,
so every SDK matches the wire protocol exactly and stays in step with it across
releases.

## Obtaining the SDKs

The SDKs live under `sdk/<language>` in the repository. Until packages are
published to the language registries, vendor the relevant directory into a
project or depend on it in place:

| Language | Location | Build / import |
|---|---|---|
| TypeScript | `sdk/typescript` | a `package.json` module (bun / npm) |
| Python | `sdk/python` | a `pyproject.toml` package (`pip install`) |
| Go | `sdk/go` | module `github.com/wangyingsm/dr-strange/sdk/go` |
| Java | `sdk/java` | a Maven module (Jackson + JDK HttpClient) |
| C | `sdk/c` | `make` → `libdrsg.a` + `drsg.h` (libcurl + json-c) |

## Connecting and calling

A client is constructed from a base URL and a token; the token defaults to the
`DRSG_TOKEN` environment variable and rides each request as an
`Authorization: Bearer` credential. Method names mirror the RPC method one to
one, adapted to each language's convention:

| Language | Construct a client | Example call |
|---|---|---|
| TypeScript | `new Drsg({ baseUrl, token })` | `await db.nodeCreate({ … })` |
| Python | `Drsg(base_url=…, token=…)` | `db.node_create(…)` |
| Go | `drsg.New(drsg.WithBaseURL(…), drsg.WithToken(…))` | `db.NodeCreate(ctx, …)` |
| Java | `new Drsg(baseUrl, token)` | `db.nodeCreate(…)` |
| C | `drsg_client_new(base_url, token)` | `drsg_node_create(…)` |

The shape is uniform. In TypeScript:

```typescript
import { Drsg } from "drsg";

const db = new Drsg({ baseUrl: "http://127.0.0.1:7700", token: process.env.DRSG_TOKEN });

await db.nodeCreate({ plane: "social", key: "ada", labels: ["Person"] });
await db.nodeCreate({ plane: "social", key: "alan", labels: ["Person"] });
await db.edgeCreate({ plane: "social", src: "ada", dst: "alan", type: "KNOWS" });

const stats = await db.dbStats();
console.log(stats.nodes, stats.edges);
```

The other languages follow the same method surface with their own idioms — Go
threads a `context.Context` through each call, Python and Java raise exceptions,
and C returns a `json_object` the caller owns and reports failure through an
out-parameter.

## Error handling

An application-level failure (an unknown plane, a malformed plan) is a JSON-RPC
error; a rejected credential is code `-32001`. The SDKs surface this as a typed
error: `DrsgError` / `DrsgAuthError` in TypeScript and Python, a `*drsg.Error`
with `IsAuthError` in Go, `DrsgException` / `DrsgAuthException` in Java, and a
filled `drsg_error` (with `drsg_is_auth_error`) in C.

## The change feed

Every SDK can open a long-lived WebSocket and subscribe to a plane's change feed
([Chapter 3](./ai-native.md)), receiving each committed `ChangeEvent`
—`{ plane, seq, truncated, changes }`, where each change is
`{ kind, op, id, labels?, record? }`. The subscription follows each language's
natural concurrency model:

**TypeScript** — a callback; the socket auto-reconnects. `close()` stops it.

```typescript
const sub = db.watch("social", (event) => {
  for (const c of event.changes) console.log(event.seq, c.op, c.kind, c.id);
});
// sub.close();
```

**Python** — a blocking generator; iterate to consume, break to disconnect.

```python
for event in db.watch("social"):
    for c in event["changes"]:
        print(event["seq"], c["op"], c["kind"], c["id"])
```

**Go** — a channel; cancel the context to stop.

```go
events, _ := db.Watch(ctx, "social")
for e := range events {
    for _, c := range e.Changes {
        fmt.Println(e.Seq, c.Op, c.Kind, c.ID)
    }
}
```

**Java** — a listener; the returned `Subscription` closes it.

```java
var sub = db.watch("social", null, event -> {
    for (var c : event.changes()) System.out.println(event.seq() + " " + c.op() + " " + c.kind());
});
// sub.close();
```

**C** — a callback; `drsg_watch` blocks until the callback returns non-zero (run
it on a thread if needed).

```c
static int on_change(struct json_object *event, void *userdata) {
    /* inspect event["changes"]; return non-zero to stop */
    return 0;
}
drsg_error err;
drsg_watch(client, "social", NULL, on_change, NULL, &err);
```

An optional label narrows the subscription to changes to nodes of that label.
Delivery is best-effort: a subscriber that falls too far behind drops the
overflow rather than stalling writers.

Because each event carries the commit sequence it landed at, a subscriber can
read the graph `as_of` that sequence — and `as_of` the one before — to
reconstruct the exact before/after of a change ([Chapter
4](./query-language.md)).

## Codegen

The typed method surface of each SDK is generated from
`crates/dr-strange-web/openrpc.json`, the single source of truth the server also
returns from `rpc.discover`. Each SDK carries a small code generator and a drift
test that fails if the committed client no longer matches the schema, so the
libraries cannot silently diverge from the wire protocol. The hand-written parts
— the transport, error types, and the WebSocket `watch` — sit beneath the
generated surface.
