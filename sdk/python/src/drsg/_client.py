"""Base JSON-RPC 2.0 transport for dr-strange (`drsg serve`).

Zero runtime dependencies — just the standard library. The typed method surface
lives in the generated ``_generated.py`` (see ``codegen.py``); this module is
the hand-written core it builds on.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from typing import Any

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
