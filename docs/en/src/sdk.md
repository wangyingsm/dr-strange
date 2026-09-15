# SDK

Dr Strange ships client libraries for **six languages** — TypeScript, Python,
Go, Java, C, and Zig. Each communicates with a running `drsg serve` over
JSON-RPC 2.0, and its typed method surface is **generated from the server's
OpenRPC schema**, so every SDK matches the wire protocol exactly and stays in
step with it across releases. (The Zig client is a thin idiomatic binding over
the generated C client, so it inherits the same guarantee.)

## Obtaining the SDKs

The SDKs live under `sdk/<language>` in the repository, all at version 2.7.0
in step with the workspace. **None is published to a language registry yet**
(no PyPI, npm or Maven Central release, no `sdk/go/vX` tags), so each is
installed from a checkout of this repository:

| Language | Location | Install from the repository |
|---|---|---|
| TypeScript | `sdk/typescript` | `bun install && bun run build` there, then `"drsg": "file:…/sdk/typescript"` in `package.json` |
| Python | `sdk/python` | `pip install …/sdk/python` (or `pip install "drsg @ git+https://github.com/wangyingsm/dr-strange.git#subdirectory=sdk/python"`) |
| Go | `sdk/go` | `go get github.com/wangyingsm/dr-strange/sdk/go@<commit>`, or a `replace … => …/sdk/go` directive |
| Java | `sdk/java` | `./mvnw install` there, then depend on `io.github.wangyingsm:drsg:2.7.0` |
| C | `sdk/c` | `make` → `libdrsg.a` + `include/drsg.h` (needs libcurl + json-c) |
| Zig | `sdk/zig` | `zig build`, or add `src/drsg.zig` as a module (Zig 0.16) |

Each directory's `README.md` gives the exact steps for its language.

## Connecting and calling

A client is constructed from a base URL and a token; the token defaults to the
`DRSG_TOKEN` environment variable and rides each request — the WebSocket
upgrade behind the change feed included — as an `Authorization: Bearer`
credential, never in a URL. (The TypeScript client falls back to `?token=` on
the socket where the standard `WebSocket` constructor cannot set headers — a
browser window or worker, and Deno; Bun and Node send the header.) Method names mirror the RPC method one to
one, adapted to each language's convention:

| Language | Construct a client | Example call |
|---|---|---|
| TypeScript | `new Drsg({ baseUrl, token })` | `await db.nodeCreate({ … })` |
| Python | `Drsg(base_url=…, token=…)` | `db.node_create(…)` |
| Go | `drsg.New(drsg.WithBaseURL(…), drsg.WithToken(…))` | `db.NodeCreate(ctx, …)` |
| Java | `new Drsg(baseUrl, token)` | `db.nodeCreate(…)` |
| C | `drsg_client_new(base_url, token)` | `drsg_node_create(…)` |
| Zig | `try drsg.Client.init(base_url, token)` | `c.drsg_node_create(client.handle, …)` |

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
error: `DrsgError` / `DrsgAuthError` in TypeScript and Python (plus
`DrsgTimeoutError` for a request that outlives the TypeScript client's
`timeoutMs`, and `DrsgProtocolError` for a non-JSON-RPC reply or a malformed
change-feed frame in Python), a `*drsg.Error`
with `IsAuthError` in Go, `DrsgException` / `DrsgAuthException` in Java, and a
filled `drsg_error` (with `drsg_is_auth_error`) in C.

## The change feed

Every SDK except Zig can open a long-lived WebSocket and subscribe to a plane's
change feed ([Chapter 3](./ai-native.md)), receiving each committed
`ChangeEvent` —`{ plane, seq, truncated, changes }`, where each change is
`{ kind, op, id, labels?, record? }`. (The Zig binding wraps the C client's
request/response surface only; a Zig program that needs the feed calls
`drsg_watch` from the C library directly.) The subscription follows each
language's natural concurrency model:

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

**Go** — a channel; cancel the context to stop. The dial and upgrade honour
the context too (30 s if it carries no deadline), the channel closes when the
server hangs up, and a message over `drsg.MaxFrameBytes` ends the watch.

```go
events, _ := db.Watch(ctx, "social")
for e := range events {
    for _, c := range e.Changes {
        fmt.Println(e.Seq, c.Op, c.Kind, c.ID)
    }
}
```

**Java** — a listener; the returned `Subscription` closes it (sending a close
frame, then aborting the socket so a silent peer cannot hold it open). A
listener that throws is logged at `WARNING` and the feed continues. The client
itself is `AutoCloseable`: `close()` ends its subscriptions and reaps the
`HttpClient` it built, unless one was shared through `Client.Options`.

```java
var sub = db.watch("social", null, event -> {
    for (var c : event.changes()) System.out.println(event.seq() + " " + c.op() + " " + c.kind());
});
// sub.close();
```

**C** — a callback; `drsg_watch` blocks until the callback returns non-zero (run
it on a thread if needed; `drsg_watch_cancellable` takes a `drsg_watch_ctl`
that another thread can `drsg_watch_ctl_cancel`). A `drsg_client` wraps one
libcurl handle and must not be used by two threads at once, so the watch thread
needs a client of its own. A message over `DRSG_WS_MAX_MESSAGE_BYTES` ends the
watch with `DRSG_TRANSPORT_ERROR_CODE`.

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
