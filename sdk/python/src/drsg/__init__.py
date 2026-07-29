"""Python client for dr-strange (`drsg serve` JSON-RPC 2.0).

    from drsg import Drsg

    db = Drsg(base_url="http://127.0.0.1:7700", token="…")  # token defaults to $DRSG_TOKEN
    db.node_create(plane="startup", key="alice", labels=["Person"])
    db.edge_create(plane="startup", src="alice", dst="bob", type="KNOWS")
    print(db.db_stats())

The method surface is generated from the server's OpenRPC schema
(`crates/dr-strange-web/openrpc.json`), so it always matches the wire protocol.
"""

from ._client import DrsgAuthError, DrsgError
from ._generated import Drsg

__all__ = ["Drsg", "DrsgError", "DrsgAuthError"]
__version__ = "0.1.0"
