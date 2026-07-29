# drsg — Go client for dr-strange

A zero-dependency (standard library only) client for a `drsg serve` JSON-RPC
endpoint. The method surface and its types are **generated from the server's
OpenRPC schema** (`crates/dr-strange-web/openrpc.json`), so they always match
the wire protocol.

## Install

```bash
go get github.com/wangyingsm/dr-strange/sdk/go@latest
```

```go
import drsg "github.com/wangyingsm/dr-strange/sdk/go"
```

## Use

```go
ctx := context.Background()

// base URL defaults to http://127.0.0.1:7700; token defaults to $DRSG_TOKEN
db := drsg.New(drsg.WithBaseURL("http://127.0.0.1:7700"), drsg.WithToken("…"))

db.NodeCreate(ctx, drsg.NodeCreateParams{Plane: "startup", Key: ptr("alice"), Labels: []string{"Person"}})
db.NodeCreate(ctx, drsg.NodeCreateParams{Plane: "startup", Key: ptr("bob"), Labels: []string{"Person"}})
db.EdgeCreate(ctx, drsg.EdgeCreateParams{Plane: "startup", Src: "alice", Dst: "bob", Type: "KNOWS"})

db.NodeUpdate(ctx, drsg.NodeUpdateParams{Plane: "startup", Key: ptr("alice"), Set: drsg.Properties{"age": 41}})
alice, _ := db.NodeGet(ctx, drsg.NodeGetParams{Plane: "startup", Key: ptr("alice")}) // *NodeRecord (nil if absent)
stats, _ := db.DbStats(ctx)
```

Each method is the RPC method PascalCased (`node.create` → `NodeCreate`,
`plane.set_props` → `PlaneSetProps`), taking a `context.Context` and a typed
`…Params` struct, and returning the typed result. Optional fields are pointers
(`ptr("alice")`) so unset is distinguishable from zero; a node reference
(`Src`/`Dst`) is an `int64` id or a `string` key.

### Auth

The whole surface is authenticated. Set a token via `WithToken` or the
`DRSG_TOKEN` environment variable; it rides each request as
`Authorization: Bearer …`. On a missing/invalid credential the call returns a
`*drsg.Error` with `Code == -32001`; test it with `drsg.IsAuthError(err)`.

## Discover

`db.RpcDiscover(ctx)` returns the server's live OpenRPC document.

## Develop

The client is generated. After editing the schema:

```bash
cd sdk/go
go generate ./...     # regenerate generated.go
go test ./...         # spins up a real drsg serve (needs the built binary)
```

`TestGeneratedIsCurrent` fails if the committed client has drifted from the
schema. The e2e suite skips (does not fail) if no `drsg` binary is found; point
it at one with `$DRSG_BIN`, or build with `cargo build -p dr-strange-cli`.
