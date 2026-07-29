#!/usr/bin/env python3
"""Generate the typed dr-strange client from the OpenRPC schema.

Schema-first: ``crates/dr-strange-web/openrpc.json`` is the single source of
truth. This emits ``src/drsg/_generated.py`` — one method per RPC method, named
snake_case (``node.create`` -> ``node_create``), with the schema's params as
keyword arguments (required first, then optionals defaulting to ``None`` and
omitted from the wire call when unset).

Run ``python codegen.py`` after editing the schema; ``test_generated.py`` fails
if the committed output has drifted from the schema.
"""

from __future__ import annotations

import json
import pathlib

HERE = pathlib.Path(__file__).resolve().parent
SCHEMA = HERE.parent.parent / "crates" / "dr-strange-web" / "openrpc.json"
OUT = HERE / "src" / "drsg" / "_generated.py"

HEADER = '''\
# This file is GENERATED from crates/dr-strange-web/openrpc.json by codegen.py.
# Do not edit by hand — run `python codegen.py` to regenerate.
# ruff: noqa: A002  (params mirror wire names: id/type/set shadow builtins)
"""Typed dr-strange client, generated from the OpenRPC schema."""

from __future__ import annotations

from typing import Any

from ._client import _Client


class Drsg(_Client):
    """A dr-strange server client — one method per JSON-RPC method.

    ``Drsg(base_url=…, token=…)``; the token defaults to ``$DRSG_TOKEN``.
    """
'''


def _method_name(rpc_name: str) -> str:
    return rpc_name.replace(".", "_")


def _render_method(m: dict) -> str:
    name = _method_name(m["name"])
    params = m.get("params", [])
    required = [p for p in params if p.get("required")]
    optional = [p for p in params if not p.get("required")]

    args = ["self"]
    args += [p["name"] for p in required]
    args += [f"{p['name']}=None" for p in optional]
    sig = ", ".join(args)

    summary = m.get("summary", "").strip()
    access = m.get("x-access", "")
    doc = summary + (f"\n\n        Access: {access}." if access else "")

    lines = [f"    def {name}({sig}) -> Any:"]
    lines.append(f'        """{doc}"""')
    if required or optional:
        lines.append("        _p: dict = {}")
        for p in required:
            lines.append(f'        _p["{p["name"]}"] = {p["name"]}')
        for p in optional:
            lines.append(f"        if {p['name']} is not None:")
            lines.append(f'            _p["{p["name"]}"] = {p["name"]}')
        lines.append(f'        return self._call("{m["name"]}", _p)')
    else:
        lines.append(f'        return self._call("{m["name"]}")')
    return "\n".join(lines)


def render(doc: dict) -> str:
    """Render the full generated module source for an OpenRPC document."""
    methods = doc.get("methods", [])
    body = "\n\n".join(_render_method(m) for m in methods)
    return HEADER + "\n" + body + "\n"


def main() -> None:
    doc = json.loads(SCHEMA.read_text())
    OUT.write_text(render(doc))
    print(f"wrote {OUT.relative_to(HERE)} ({len(doc['methods'])} methods)")


if __name__ == "__main__":
    main()
