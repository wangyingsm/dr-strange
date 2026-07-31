"""Base JSON-RPC 2.0 transport for dr-strange (`drsg serve`).

Zero runtime dependencies — just the standard library. The typed method surface
lives in the generated ``_generated.py`` (see ``codegen.py``); this module is
the hand-written core it builds on.
"""

from __future__ import annotations

import base64
import json
import os
import socket
import ssl
import struct
import urllib.error
import urllib.request
from collections.abc import Iterator
from typing import Any
from urllib.parse import quote, urlsplit

DEFAULT_BASE_URL = "http://127.0.0.1:7700"


class DrsgError(Exception):
    """A JSON-RPC error returned by the server (carries the numeric ``code``)."""

    def __init__(self, code: int, message: str, data: Any = None) -> None:
        super().__init__(f"{message} (code {code})")
        self.code = code
        self.message = message
        self.data = data


class DrsgAuthError(DrsgError):
    """The server rejected the credential (code ``-32001``).

    Set a valid token: ``Drsg(token=…)`` or the ``DRSG_TOKEN`` environment
    variable. With no token configured server-side, only the same-origin browser
    UI is authorized — a programmatic client must present one.
    """


class _Client:
    """One endpoint's worth of config plus the JSON-RPC call primitive.

    The token defaults to the ``DRSG_TOKEN`` environment variable (mirroring the
    server), or pass it explicitly. It rides every request as
    ``Authorization: Bearer <token>``.
    """

    def __init__(
        self,
        base_url: str = DEFAULT_BASE_URL,
        token: str | None = None,
        timeout: float = 30.0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.token = token if token is not None else os.environ.get("DRSG_TOKEN")
        self.timeout = timeout
        self._id = 0

    def _call(self, method: str, params: dict | None = None) -> Any:
        self._id += 1
        payload: dict[str, Any] = {"jsonrpc": "2.0", "method": method, "id": self._id}
        if params:
            payload["params"] = params
        body = json.dumps(payload).encode("utf-8")
        headers = {"content-type": "application/json"}
        if self.token:
            headers["authorization"] = f"Bearer {self.token}"

        req = urllib.request.Request(  # noqa: S310 — fixed http(s) endpoint
            f"{self.base_url}/rpc", data=body, headers=headers, method="POST"
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:  # noqa: S310
                raw = resp.read()
        except urllib.error.HTTPError as e:
            # A transport-level refusal (e.g. 403 cross-origin, 413 too large)
            # arrives as HTTP, not a JSON-RPC error — surface it uniformly.
            raise DrsgError(-32000, f"HTTP {e.code}: {e.reason}") from e
        except urllib.error.URLError as e:
            raise DrsgError(-32000, f"connection failed: {e.reason}") from e

        msg = json.loads(raw)
        if "error" in msg:
            err = msg["error"]
            code = int(err.get("code", -32000))
            cls = DrsgAuthError if code == -32001 else DrsgError
            raise cls(code, err.get("message", "error"), err.get("data"))
        return msg.get("result")

    def watch(self, plane: str, *, label: str | None = None) -> Iterator[dict]:
        """Yield each committed change for ``plane`` as it lands (ROADMAP §5).

        Opens a long-lived WebSocket, subscribes with ``plane.watch``, and yields
        one dict per commit::

            for event in db.watch("startup"):
                for c in event["changes"]:
                    print(event["seq"], c["op"], c["kind"], c["id"])

        Each event is ``{plane, seq, truncated, changes: [{kind, op, id,
        labels?, record?}]}``; ``record`` (a create/update) has embeddings and
        ``_``-prefixed props stripped, and a delete carries only ``id`` (read
        ``as_of=seq-1`` for the before-state). Pass ``label`` to receive only
        node changes carrying it.

        A blocking generator: iterate to consume, break/close to disconnect.
        Best-effort — a slow consumer can miss commits, and reconnect (after a
        drop) is the caller's to add by re-entering the loop.
        """
        ws = _WebSocket.connect(self.base_url, self.token, self.timeout)
        params: dict[str, Any] = {"plane": plane}
        if label:
            params["label"] = label
        ws.send_text(json.dumps({"jsonrpc": "2.0", "method": "plane.watch", "params": params}))
        try:
            while True:
                text = ws.recv_text()
                if text is None:
                    break
                try:
                    msg = json.loads(text)
                except ValueError:
                    continue
                if msg.get("method") == "plane.change":
                    yield msg["params"]
        finally:
            ws.close()


class _WebSocket:
    """A minimal RFC 6455 text-frame client — just enough to consume the change
    feed (ROADMAP §5). Zero dependencies: raw stdlib sockets, since Python has no
    built-in WebSocket. Client frames are masked; server frames are read,
    de-fragmented, and pings are answered."""

    def __init__(self, sock: socket.socket) -> None:
        self._sock = sock
        self._buf = b""

    @classmethod
    def connect(cls, base_url: str, token: str | None, timeout: float) -> _WebSocket:
        parts = urlsplit(base_url)
        secure = parts.scheme == "https"
        host = parts.hostname or "127.0.0.1"
        port = parts.port or (443 if secure else 80)
        path = "/ws" + (f"?token={quote(token)}" if token else "")

        raw = socket.create_connection((host, port), timeout=timeout)
        sock = (
            ssl.create_default_context().wrap_socket(raw, server_hostname=host)
            if secure
            else raw
        )
        key = base64.b64encode(os.urandom(16)).decode()
        handshake = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        sock.sendall(handshake.encode())

        resp = b""
        while b"\r\n\r\n" not in resp:
            chunk = sock.recv(4096)
            if not chunk:
                raise DrsgError(-32000, "websocket handshake failed (connection closed)")
            resp += chunk
        head, rest = resp.split(b"\r\n\r\n", 1)
        status = head.split(b"\r\n", 1)[0]
        if b" 101 " not in status:
            raise DrsgError(-32000, f"websocket upgrade refused: {status.decode('latin1')}")

        ws = cls(sock)
        ws._buf = rest  # bytes after the header start the first frame(s)
        sock.settimeout(None)  # the watch loop blocks indefinitely for events
        return ws

    def send_text(self, text: str) -> None:
        payload = text.encode("utf-8")
        header = bytearray([0x81])  # FIN + text opcode
        n = len(payload)
        if n < 126:
            header.append(0x80 | n)
        elif n < 65536:
            header.append(0x80 | 126)
            header += struct.pack("!H", n)
        else:
            header.append(0x80 | 127)
            header += struct.pack("!Q", n)
        mask = os.urandom(4)
        header += mask
        self._sock.sendall(bytes(header) + _xor(payload, mask))

    def recv_text(self) -> str | None:
        """The next complete text message, or ``None`` when the socket closes."""
        message = b""
        while True:
            head = self._read(2)
            if len(head) < 2:
                return None
            fin = head[0] & 0x80
            opcode = head[0] & 0x0F
            masked = head[1] & 0x80
            length = head[1] & 0x7F
            if length == 126:
                length = struct.unpack("!H", self._read(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", self._read(8))[0]
            mask = self._read(4) if masked else b""
            data = self._read(length) if length else b""
            if masked and data:
                data = _xor(data, mask)
            if opcode == 0x8:  # close
                return None
            if opcode == 0x9:  # ping → pong
                self._send_control(0xA, data)
                continue
            if opcode == 0xA:  # pong
                continue
            message += data  # text (0x1) or continuation (0x0)
            if fin:
                return message.decode("utf-8", "replace")

    def _read(self, n: int) -> bytes:
        while len(self._buf) < n:
            chunk = self._sock.recv(65536)
            if not chunk:
                return b""  # closed mid-frame
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def _send_control(self, opcode: int, payload: bytes) -> None:
        mask = os.urandom(4)
        frame = bytes([0x80 | opcode, 0x80 | len(payload)]) + mask + _xor(payload, mask)
        self._sock.sendall(frame)

    def close(self) -> None:
        try:
            self._send_control(0x8, b"")  # close frame
        except OSError:
            pass
        try:
            self._sock.close()
        except OSError:
            pass


def _xor(data: bytes, mask: bytes) -> bytes:
    return bytes(b ^ mask[i % 4] for i, b in enumerate(data))
