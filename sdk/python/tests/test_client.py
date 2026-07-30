"""End-to-end tests: drive a real `drsg serve` over the client.

Requires the `drsg` binary. Point at it with $DRSG_BIN, else the workspace
`target/{debug,release}/drsg` is used; the suite skips if none is found.
"""

from __future__ import annotations

import os
import pathlib
import socket
import subprocess
import tempfile
import time

import pytest

from drsg import Drsg, DrsgAuthError

TOKEN = "test-token"
ROOT = pathlib.Path(__file__).resolve().parents[3]


def _find_binary() -> str | None:
    if env := os.environ.get("DRSG_BIN"):
        return env if pathlib.Path(env).exists() else None
    for profile in ("debug", "release"):
        cand = ROOT / "target" / profile / "drsg"
        if cand.exists():
            return str(cand)
    return None


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture(scope="module")
def base_url():
    binary = _find_binary()
    if not binary:
        pytest.skip("drsg binary not found; run `cargo build -p dr-strange-cli`")
    port = _free_port()
    tmp = tempfile.mkdtemp()
    db = pathlib.Path(tmp) / "sdk-test.drsg"
    proc = subprocess.Popen(
        [binary, "--db", str(db), "serve", "--addr", f"127.0.0.1:{port}"],
        env={**os.environ, "DRSG_TOKEN": TOKEN},
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    # Wait for the listener.
    for _ in range(100):
        try:
            with socket.create_connection(("127.0.0.1", port), 0.1):
                break
        except OSError:
            time.sleep(0.05)
    else:
        proc.terminate()
        pytest.fail("server never started listening")
    yield f"http://127.0.0.1:{port}"
    proc.terminate()
    proc.wait(timeout=5)


def test_crud_roundtrip(base_url):
    db = Drsg(base_url=base_url, token=TOKEN)
    assert db.db_stats()["nodes"] == 0

    alice = db.node_create(plane="startup", key="alice", labels=["Person"])
    assert alice["external_key"] == "alice"
    db.node_create(plane="startup", key="bob", labels=["Person"])

    edge = db.edge_create(plane="startup", src="alice", dst="bob", type="KNOWS")
    assert edge["type"] == "KNOWS"

    # Property patch: set then unset, with types preserved.
    updated = db.node_update(plane="startup", key="alice", set={"age": 41, "city": "NYC"})
    assert updated["properties"]["age"] == 41
    updated = db.node_update(plane="startup", key="alice", unset=["city"])
    assert "city" not in updated["properties"]

    got = db.node_get(plane="startup", key="alice")
    assert got["properties"]["age"] == 41

    # Delete cascades the edge; the graph is left consistent.
    assert db.node_delete(plane="startup", key="alice")["deleted"] is True
    stats = db.db_stats()
    assert (stats["nodes"], stats["edges"]) == (1, 0)


def test_plane_admin(base_url):
    db = Drsg(base_url=base_url, token=TOKEN)
    assert db.plane_create(name="notes")["name"] == "notes"
    assert db.plane_rename(plane="notes", to="archive")["name"] == "archive"
    assert db.plane_delete(plane="archive")["deleted"] is True


def test_discover(base_url):
    db = Drsg(base_url=base_url, token=TOKEN)
    doc = db.rpc_discover()
    assert doc["openrpc"] == "1.2.6"
    assert any(m["name"] == "node.create" for m in doc["methods"])


def test_cypher_read_and_write(base_url):
    db = Drsg(base_url=base_url, token=TOKEN)
    # A write: CREATE two nodes and an edge → change-counts.
    out = db.plane_cypher(
        plane="startup",
        query='CREATE (a:Person {key:"carol", age:30})-[:KNOWS]->(b:Person {key:"dave"})',
    )
    assert out["write"] is True
    assert out["nodes_created"] == 2
    assert out["edges_created"] == 1
    # A read: MATCH … RETURN returns the result subgraph.
    res = db.plane_cypher(
        plane="startup",
        query="MATCH (n:Person) WHERE n.age >= 18 RETURN n",
    )
    assert res["count"] == 1
    assert res["nodes"][0]["external_key"] == "carol"

    # $params: values passed out-of-band, no string interpolation.
    res = db.plane_cypher(
        plane="startup",
        query="MATCH (n:Person) WHERE n.age >= $min RETURN n",
        params={"min": 40},
    )
    assert res["count"] == 0  # carol is 30


def test_bad_token_raises_auth_error(base_url):
    db = Drsg(base_url=base_url, token="wrong")
    with pytest.raises(DrsgAuthError) as exc:
        db.db_stats()
    assert exc.value.code == -32001
