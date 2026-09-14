# drsg — C client for dr-strange

A C client for a `drsg serve` JSON-RPC endpoint. The method surface is
**generated from the server's OpenRPC schema**
(`crates/dr-strange-web/openrpc.json`), so it always matches the wire protocol.
HTTP uses **libcurl**; JSON uses **json-c** (values in and out are
`json_object`).

## Build

Needs `libcurl` and `json-c` development packages (found via `pkg-config`):

```bash
cd sdk/c
make            # builds libdrsg.a
```

Link against `libdrsg.a` plus the two libraries:

```bash
cc myapp.c libdrsg.a $(pkg-config --cflags --libs json-c libcurl) -pthread -Iinclude -o myapp
```

(`-pthread`: the library serialises its one-time libcurl initialisation and
guards the watch cancellation handle with pthreads.)

## Use

```c
#include "drsg.h"

// base_url NULL -> http://127.0.0.1:7700; token NULL -> $DRSG_TOKEN
drsg_client *c = drsg_client_new("http://127.0.0.1:7700", "…");
drsg_error err;

struct json_object *labels = json_object_new_array();
json_object_array_add(labels, json_object_new_string("Person"));
drsg_node_create_opts opts = { .key = "alice", .labels = labels };
struct json_object *alice = drsg_node_create(c, "startup", &opts, &err);
if (!alice) fprintf(stderr, "error %d: %s\n", err.code, err.message);
json_object_put(alice);      // caller owns every returned json_object
json_object_put(labels);

drsg_client_free(c);
```

Each method is `drsg_<method>` (dots → underscores, `node.create` →
`drsg_node_create`). **Required** params are positional C arguments; **optional**
params live in a nullable `drsg_<method>_opts` struct (a NULL field is omitted).
A node reference (`src`/`dst`) is a `json_object` — pass
`json_object_new_string(key)` or `json_object_new_int64(id)`. Every call returns
a `json_object` the caller must `json_object_put`, or `NULL` on error (with
`err` filled).

A runnable version is [`examples/quickstart.c`](examples/quickstart.c) — `make example && ./example`.

### Auth

The whole surface is authenticated. Pass a token or set `DRSG_TOKEN`; it rides
each request as `Authorization: Bearer …`. On a missing/invalid credential the
call returns `NULL` with `err.code == -32001`; test it with
`drsg_is_auth_error(&err)`.

### Threads

A `drsg_client` wraps one libcurl easy handle, which must not be used by two
threads at once: use one client per thread. `drsg_watch` blocks its caller, so
run it on a dedicated thread with a client of its own; only the one-time
library initialisation and `drsg_watch_ctl` are safe to share.

### Change feed

`drsg_watch(c, plane, label, cb, userdata, &err)` opens a WebSocket to
`<base_url>/ws` and calls `cb` for each committed change event until `cb`
returns non-zero or the server closes. To stop it from another thread, use the
cancellable form:

```c
drsg_watch_ctl *ctl = drsg_watch_ctl_new();
/* on the watch thread: */
drsg_watch_cancellable(c, "startup", NULL, on_change, NULL, ctl, &err);
/* from anywhere else, later: */
drsg_watch_ctl_cancel(ctl);          /* the watch returns 0 promptly */
drsg_watch_ctl_free(ctl);            /* after the watch has returned */
```

The client checks the server's `Sec-WebSocket-Accept`, masks every frame with
a fresh key, percent-encodes the token in the `?token=` query, and refuses any
message larger than `DRSG_WS_MAX_MESSAGE_BYTES` (64 MiB) before allocating for
it; each of those failures ends the watch with `-1` and
`err.code == DRSG_TRANSPORT_ERROR_CODE` (`-32000`) and a message that says why.

## Discover

`drsg_rpc_discover(c, &err)` returns the server's live OpenRPC document.

## Develop

The client is generated. After editing the schema:

```bash
make generate      # regenerate include/drsg_generated.h + src/drsg_generated.c
make test          # drift check + e2e against a real drsg serve
```

`make check-drift` fails if the committed client has drifted from the schema.
Besides the real server, the e2e suite runs the client against
`test/fake_ws.py`, a deliberately misbehaving WebSocket peer (oversized frame
header, forged `Sec-WebSocket-Accept`, token needing URL escaping).
The e2e suite skips (does not fail) if no `drsg` binary is found; point it at
one with `$DRSG_BIN`, or build with `cargo build -p dr-strange-cli`.
