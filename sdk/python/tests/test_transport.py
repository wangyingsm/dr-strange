"""Unit tests for the hand-written transport: no server, sockets from a pair.

The e2e suite (`test_client.py`) covers the happy paths against a real
`drsg serve`; these pin the failure edges a server would never exercise —
truncated and oversized WebSocket frames, and non-JSON replies on `/rpc`.
"""

from __future__ import annotations

import json
import os
import socket
import struct
from unittest import mock

import pytest

from drsg import DrsgError, DrsgProtocolError
from drsg._client import MAX_FRAME_BYTES, _Client, _WebSocket, _xor


def _frame(payload: bytes, *, opcode: int = 0x1, mask: bytes | None = None) -> bytes:
    """Build one FIN frame the way a server (unmasked) or client (masked) would."""
    n = len(payload)
    head = bytearray([0x80 | opcode])
    mbit = 0x80 if mask else 0
    if n < 126:
        head.append(mbit | n)
    elif n < 65536:
        head.append(mbit | 126)
        head += struct.pack("!H", n)
    else:
        head.append(mbit | 127)
        head += struct.pack("!Q", n)
    if mask:
        return bytes(head) + mask + _xor(payload, mask)
    return bytes(head) + payload


@pytest.fixture
def pair():
    a, b = socket.socketpair()
    yield a, b
    a.close()
    b.close()


def test_xor_matches_the_per_byte_definition():
    for n in (0, 1, 3, 4, 5, 17, 1024, 70_001):
        data = os.urandom(n)
        mask = os.urandom(4)
        expected = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        out = _xor(data, mask)
        assert out == expected
        assert _xor(out, mask) == data  # an involution, as masking must be


def test_recv_text_reassembles_frames_and_answers_pings(pair):
    client, server = pair
    ws = _WebSocket(client)
    server.sendall(_frame(b"ping!", opcode=0x9))
    server.sendall(b"\x01\x03hel")  # text, FIN clear
    server.sendall(b"\x80\x02lo")  # FIN + continuation
    assert ws.recv_text() == "hello"
    # The pong is a masked client frame echoing the ping payload.
    pong = server.recv(64)
    assert pong[0] == 0x8A
    assert _xor(pong[6:], pong[2:6]) == b"ping!"


def test_clean_close_between_frames_yields_none(pair):
    client, server = pair
    ws = _WebSocket(client)
    server.sendall(_frame(b"one"))
    server.close()
    assert ws.recv_text() == "one"
    assert ws.recv_text() is None


def test_truncated_frame_is_a_protocol_error(pair):
    client, server = pair
    ws = _WebSocket(client)
    whole = _frame(b"x" * 300)  # 16-bit length form
    server.sendall(whole[: len(whole) - 100])
    server.close()
    with pytest.raises(DrsgProtocolError, match="mid-frame"):
        ws.recv_text()


def test_truncated_length_header_is_a_protocol_error(pair):
    client, server = pair
    ws = _WebSocket(client)
    server.sendall(b"\x81\x7f\x00\x00\x00")  # 64-bit length announced, 3 of 8 bytes sent
    server.close()
    with pytest.raises(DrsgProtocolError):
        ws.recv_text()


def test_oversized_frame_is_refused_before_allocation(pair):
    client, server = pair
    ws = _WebSocket(client)
    # A 2^62-byte frame: the header alone must trip the cap, without the
    # client ever waiting for (or trying to buffer) the payload.
    server.sendall(b"\x81\x7f" + struct.pack("!Q", 1 << 62))
    with pytest.raises(DrsgProtocolError, match="exceeds"):
        ws.recv_text()
    assert MAX_FRAME_BYTES == 64 * 1024 * 1024


def test_cap_is_per_instance_tunable(pair):
    client, server = pair
    ws = _WebSocket(client)
    ws.max_frame_bytes = 8
    server.sendall(_frame(b"123456789"))
    with pytest.raises(DrsgProtocolError):
        ws.recv_text()


def test_protocol_error_is_a_drsg_error():
    err = DrsgProtocolError("boom")
    assert isinstance(err, DrsgError)
    assert err.code == -32000


class _Resp:
    def __init__(self, body: bytes) -> None:
        self._body = body

    def read(self) -> bytes:
        return self._body

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


@pytest.mark.parametrize("body", [b"<html>502</html>", b"", b"[1, 2]", b'"str"'])
def test_non_jsonrpc_reply_is_a_protocol_error(body):
    c = _Client(token="t")
    with mock.patch("urllib.request.urlopen", return_value=_Resp(body)):
        with pytest.raises(DrsgProtocolError, match="invalid JSON-RPC response"):
            c._call("db.stats")


def test_wellformed_reply_still_returns_the_result():
    c = _Client(token="t")
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "result": {"nodes": 3}}).encode()
    with mock.patch("urllib.request.urlopen", return_value=_Resp(body)):
        assert c._call("db.stats") == {"nodes": 3}
