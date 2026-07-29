# drsg — Zig client for dr-strange

A Zig client for a `drsg serve` JSON-RPC endpoint. It is a thin idiomatic
binding over the **C client** (`sdk/c`), which is generated from the server's
OpenRPC schema — so the typed method surface always matches the wire protocol.
`build.zig` `@cImport`s `drsg.h`, compiles the C sources directly, and links
system **libcurl** + **json-c** via `pkg-config`.

Built with **Zig 0.16**.

## Add to your build

```zig
// build.zig — add the module and wire the C library (see this repo's build.zig)
const drsg = b.addModule("drsg", .{ .root_source_file = ... });
// link_libc, add ../c/include, compile ../c/src/*.c, link json-c + libcurl
```

## Use

```zig
const drsg = @import("drsg");
const c = drsg.c; // generated functions + json-c helpers

// base_url null -> http://127.0.0.1:7700; token null -> $DRSG_TOKEN
var client = try drsg.Client.init("http://127.0.0.1:7700", "…");
defer client.deinit();

const labels = c.json_object_new_array();
defer _ = c.json_object_put(labels);
_ = c.json_object_array_add(labels, c.json_object_new_string("Person"));

var opts = std.mem.zeroes(c.drsg_node_create_opts);
opts.key = "alice";
opts.labels = labels;
const alice = c.drsg_node_create(client.handle, "startup", &opts, &client.err) orelse {
    std.debug.print("error {d}: {s}\n", .{ client.err.code, client.err.message });
    return;
};
defer _ = c.json_object_put(alice); // caller owns every returned json_object
```

The generated typed functions are `c.drsg_<method>` (see `sdk/c/README.md`):
required params are positional, optionals go in a `c.drsg_<method>_opts` struct
(a null field is omitted), and each returns an owned `*json_object`. The `Client`
wrapper adds RAII (`init`/`deinit`), a generic `call`, and Zig error unions.

A runnable version is [`examples/quickstart.zig`](examples/quickstart.zig) — `zig build example`.

### Auth

Pass a token to `Client.init` or set `DRSG_TOKEN`; it rides each request as
`Authorization: Bearer …`. On a missing/invalid credential a call returns null
with `client.err.code == -32001` (test with `c.drsg_is_auth_error(&client.err)`);
the generic `Client.call` surfaces it as `error.Unauthorized`.

## Develop

The method surface is generated in the C SDK, not here — regenerate it with
`make generate` in `sdk/c`, then this binding picks it up automatically.

```bash
cd sdk/zig
./test/run.sh     # starts a real drsg serve, runs `zig build test` against it
```

The e2e suite skips (does not fail) if no `drsg` binary is found; point it at
one with `$DRSG_BIN`, or build with `cargo build -p dr-strange-cli`. Needs
`libcurl` and `json-c` development packages.
