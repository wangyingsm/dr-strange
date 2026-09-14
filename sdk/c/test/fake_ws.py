#!/usr/bin/env python3
"""A deliberately misbehaving WebSocket server for the C e2e suite.

The real `drsg serve` is well-behaved, so the client's defences against a
hostile or broken peer cannot be exercised against it. This server picks a
script from the `Authorization: Bearer` header of the upgrade:

  big         valid handshake, then a frame whose header claims 2^40 bytes
  bad-accept  a 101 whose Sec-WebSocket-Accept does not match the key
  a&b=c#d     must arrive verbatim in the header, with a bare `/ws` target;
              then a valid handshake and a clean close, so the client returns 0

On `big` it also refuses to play if the client's subscribe frame still uses
the old fixed mask key, so the test fails if masking regresses. The header
check is made on the raw request *before* the upgrade: a target carrying any
query string (the old `?token=` form, which the server keeps only for
browsers — arch/08-web-ui §4.1), or a header that does not spell the token
exactly, is answered with `400 Bad Request`, which the client reports as
"websocket upgrade refused" (-1). The e2e's `rc == 0` therefore only holds
when the token really travelled as a header and nowhere else.
"""
import base64
import hashlib
import socket
import sys

GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
OLD_FIXED_MASK = bytes([0x37, 0xFA, 0x21, 0x3D])


def read_head(conn):
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = conn.recv(4096)
        if not chunk:
            return None
        data += chunk
    return data.split(b"\r\n\r\n", 1)[0]


def accept_for(key):
    return base64.b64encode(hashlib.sha1(key + GUID).digest()).decode()


def read_client_frame(conn):
    head = conn.recv(2)
    if len(head) < 2:
        return None, None
    length = head[1] & 0x7F
    if length == 126:
        length = int.from_bytes(conn.recv(2), "big")
    elif length == 127:
        length = int.from_bytes(conn.recv(8), "big")
    mask = conn.recv(4) if head[1] & 0x80 else None
    payload = b""
    while len(payload) < length:
        payload += conn.recv(length - len(payload))
    return mask, payload


def serve_one(conn):
    head = read_head(conn)
    if head is None:
        return
    request_line = head.split(b"\r\n", 1)[0].decode()
    target = request_line.split(" ")[1]
    key = b""
    token = ""
    for line in head.split(b"\r\n")[1:]:
        name, _, value = line.partition(b":")
        if name.strip().lower() == b"sec-websocket-key":
            key = value.strip()
        elif name.strip().lower() == b"authorization":
            token = value.strip().decode()
            token = token[len("Bearer "):] if token.startswith("Bearer ") else ""
    if target != "/ws" or token.startswith("a") and token != "a&b=c#d":
        # A credential in the URL, or a metacharacter token that did not
        # arrive verbatim in the header: refuse before upgrading so drsg_watch
        # fails instead of ending cleanly.
        conn.sendall(
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n"
            b"Connection: close\r\n\r\n"
        )
        return
    accept = accept_for(key)
    if token == "bad-accept":
        accept = accept_for(b"not-the-key")
    conn.sendall(
        (
            "HTTP/1.1 101 Switching Protocols\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
        ).encode()
    )
    if token == "bad-accept":
        return
    mask, _ = read_client_frame(conn)  # the plane.watch subscribe frame
    if token == "big" and mask is not None and mask != OLD_FIXED_MASK:
        # Text frame, FIN, 64-bit length of 2^40: the client must refuse it
        # before reading (or allocating) a single payload byte.
        conn.sendall(bytes([0x81, 127]) + (1 << 40).to_bytes(8, "big"))
        try:
            conn.recv(1)  # wait for the client to hang up
        except OSError:
            pass
        return
    conn.sendall(bytes([0x88, 0]))  # clean close


def main():
    port = int(sys.argv[1])
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port))
    srv.listen(8)
    while True:
        conn, _ = srv.accept()
        try:
            serve_one(conn)
        except OSError:
            pass
        finally:
            conn.close()


if __name__ == "__main__":
    main()
