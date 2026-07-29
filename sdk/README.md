# dr-strange client SDKs

Client libraries for a `drsg serve` JSON-RPC endpoint, in six languages. Every
SDK is **schema-first** — its method surface is generated from the server's
OpenRPC schema (`crates/dr-strange-web/openrpc.json`, also served live from
`rpc.discover`) — so it always matches the wire protocol.

| Language | Directory | Runtime deps | Install / build |
|---|---|---|---|
| Python | [`python/`](python) | none (stdlib) | `uv pip install -e sdk/python` |
| TypeScript | [`typescript/`](typescript) | none (platform `fetch`) | `bun add drsg` |
| Go | [`go/`](go) | none (stdlib) | `go get github.com/wangyingsm/dr-strange/sdk/go` |
| Java | [`java/`](java) | Jackson | Maven `io.github.wangyingsm:drsg` |
| C | [`c/`](c) | libcurl, json-c | `make` (→ `libdrsg.a`) |
| Zig | [`zig/`](zig) | binds the C client | `zig build` |

All are authenticated: pass a token or set `DRSG_TOKEN`; it rides each request as
`Authorization: Bearer …`, and a missing/invalid credential surfaces as a typed
auth error (code `-32001`). See each directory's `README.md` for full docs.

## Quickstart

The same program in every language: create two people on the `startup` plane,
link them, set a property, read it back, and print the graph size. Each has a
runnable copy under `<lang>/examples/`. Run it against a `drsg serve` on
`127.0.0.1:7700` with `DRSG_TOKEN` set (or no token for a zero-config local UI).

Expected output:

```
alice.age = 30
2 nodes, 1 edge(s)
```

### Python — `python examples/quickstart.py`

```python
from drsg import Drsg

db = Drsg()  # base http://127.0.0.1:7700; token from $DRSG_TOKEN

db.node_create(plane="startup", key="alice", labels=["Person"])
db.node_create(plane="startup", key="bob", labels=["Person"])
db.edge_create(plane="startup", src="alice", dst="bob", type="KNOWS")
db.node_update(plane="startup", key="alice", set={"age": 30})

alice = db.node_get(plane="startup", key="alice")
print(f"alice.age = {alice['properties']['age']}")

stats = db.db_stats()
print(f"{stats['nodes']} nodes, {stats['edges']} edge(s)")
```

### TypeScript — `bun examples/quickstart.ts`

```ts
import { Drsg } from "drsg";

const db = new Drsg(); // base http://127.0.0.1:7700; token from $DRSG_TOKEN

await db.nodeCreate({ plane: "startup", key: "alice", labels: ["Person"] });
await db.nodeCreate({ plane: "startup", key: "bob", labels: ["Person"] });
await db.edgeCreate({ plane: "startup", src: "alice", dst: "bob", type: "KNOWS" });
await db.nodeUpdate({ plane: "startup", key: "alice", set: { age: 30 } });

const alice = await db.nodeGet({ plane: "startup", key: "alice" });
console.log(`alice.age = ${alice?.properties.age}`);

const stats = await db.dbStats();
console.log(`${stats.nodes} nodes, ${stats.edges} edge(s)`);
```

### Go — `go run ./examples`

```go
ctx := context.Background()
db := drsg.New() // base http://127.0.0.1:7700; token from $DRSG_TOKEN

db.NodeCreate(ctx, drsg.NodeCreateParams{Plane: "startup", Key: ptr("alice"), Labels: []string{"Person"}})
db.NodeCreate(ctx, drsg.NodeCreateParams{Plane: "startup", Key: ptr("bob"), Labels: []string{"Person"}})
db.EdgeCreate(ctx, drsg.EdgeCreateParams{Plane: "startup", Src: "alice", Dst: "bob", Type: "KNOWS"})
db.NodeUpdate(ctx, drsg.NodeUpdateParams{Plane: "startup", Key: ptr("alice"), Set: drsg.Properties{"age": 30}})

alice, _ := db.NodeGet(ctx, drsg.NodeGetParams{Plane: "startup", Key: ptr("alice")})
fmt.Printf("alice.age = %v\n", alice.Properties["age"])

stats, _ := db.DbStats(ctx)
fmt.Printf("%d nodes, %d edge(s)\n", stats.Nodes, stats.Edges)
```

### Java — `javac … Quickstart.java && java … Quickstart`

```java
Drsg db = new Drsg(); // base http://127.0.0.1:7700; token from $DRSG_TOKEN

db.nodeCreate(Drsg.NodeCreateParams.of("startup").withKey("alice").withLabels(List.of("Person")));
db.nodeCreate(Drsg.NodeCreateParams.of("startup").withKey("bob").withLabels(List.of("Person")));
db.edgeCreate(Drsg.EdgeCreateParams.of("startup", "alice", "bob", "KNOWS"));
db.nodeUpdate(Drsg.NodeUpdateParams.of("startup").withKey("alice").withSet(Map.of("age", 30)));

Drsg.NodeRecord alice = db.nodeGet(Drsg.NodeGetParams.of("startup").withKey("alice"));
System.out.println("alice.age = " + alice.properties().get("age"));

Drsg.DbStats stats = db.dbStats();
System.out.println(stats.nodes() + " nodes, " + stats.edges() + " edge(s)");
```

### C — `make example && ./example`

```c
drsg_client *c = drsg_client_new(NULL, NULL); /* :7700; token from $DRSG_TOKEN */
drsg_error err;

struct json_object *labels = json_object_new_array();
json_object_array_add(labels, json_object_new_string("Person"));
drsg_node_create_opts alice_opts = {.key = "alice", .labels = labels};
json_object_put(drsg_node_create(c, "startup", &alice_opts, &err));
drsg_node_create_opts bob_opts = {.key = "bob", .labels = labels};
json_object_put(drsg_node_create(c, "startup", &bob_opts, &err));
json_object_put(labels);

struct json_object *src = json_object_new_string("alice");
struct json_object *dst = json_object_new_string("bob");
json_object_put(drsg_edge_create(c, "startup", src, dst, "KNOWS", NULL, &err));
json_object_put(src); json_object_put(dst);

/* … set age=30, read it back, print db.stats — see c/examples/quickstart.c … */
drsg_client_free(c);
```

### Zig — `zig build example`

```zig
const drsg = @import("drsg");
const c = drsg.c;

var client = try drsg.Client.init(null, null); // :7700; token from $DRSG_TOKEN
defer client.deinit();
const h = client.handle;

var alice_opts = std.mem.zeroes(c.drsg_node_create_opts);
alice_opts.key = "alice";
_ = c.json_object_put(c.drsg_node_create(h, "startup", &alice_opts, &client.err));
// … create bob, the KNOWS edge, set age=30, read it back — see zig/examples/quickstart.zig …
```
