"""Minimal dr-strange quickstart — run against a `drsg serve` on :7700.

    DRSG_TOKEN=… uv run --project sdk/python python sdk/python/examples/quickstart.py
"""

from drsg import Drsg


def main() -> None:
    db = Drsg()  # base http://127.0.0.1:7700; token from $DRSG_TOKEN

    db.node_create(plane="startup", key="alice", labels=["Person"])
    db.node_create(plane="startup", key="bob", labels=["Person"])
    db.edge_create(plane="startup", src="alice", dst="bob", type="KNOWS")
    db.node_update(plane="startup", key="alice", set={"age": 30})

    alice = db.node_get(plane="startup", key="alice")
    print(f"alice.age = {alice['properties']['age']}")

    stats = db.db_stats()
    print(f"{stats['nodes']} nodes, {stats['edges']} edge(s)")


if __name__ == "__main__":
    main()
