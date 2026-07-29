# drsg — Python client for dr-strange

A zero-dependency (standard library only) client for a `drsg serve` JSON-RPC
endpoint. The method surface is **generated from the server's OpenRPC schema**
(`crates/dr-strange-web/openrpc.json`), so it always matches the wire protocol.

## Install

```bash
uv pip install -e sdk/python      # or: pip install -e sdk/python
```

## Use

```python
from drsg import Drsg, DrsgError, DrsgAuthError

# token defaults to $DRSG_TOKEN; base_url defaults to http://127.0.0.1:7700
db = Drsg(base_url="http://127.0.0.1:7700", token="…")

db.node_create(plane="startup", key="alice", labels=["Person"])
db.node_create(plane="startup", key="bob", labels=["Person"])
db.edge_create(plane="startup", src="alice", dst="bob", type="KNOWS")

db.node_update(plane="startup", key="alice", set={"age": 41})
print(db.node_get(plane="startup", key="alice"))
print(db.db_stats())
```

Every method name is the RPC method with `.` → `_` (`node.create` →
`node_create`); parameters are the schema's, keyword-friendly, with optionals
defaulting to `None` and omitted from the call when unset.

A runnable version is [`examples/quickstart.py`](examples/quickstart.py) — `python examples/quickstart.py`.

### Auth

The whole surface is authenticated. Pass `token=` or set `DRSG_TOKEN`; it rides
each request as `Authorization: Bearer …`. A missing/invalid credential raises
`DrsgAuthError` (code `-32001`); other server errors raise `DrsgError` with a
`.code`.

## Discover

`db.rpc_discover()` returns the server's live OpenRPC document.

## Develop

The client is generated. After editing the schema:

```bash
cd sdk/python
python codegen.py          # regenerate src/drsg/_generated.py
uv run pytest              # spins up a real drsg serve (needs the built binary)
```

`test_generated.py` fails if the committed client has drifted from the schema.
