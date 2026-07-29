"""The committed generated client must match the schema (no manual drift)."""

from __future__ import annotations

import json

import codegen


def test_generated_is_current():
    doc = json.loads(codegen.SCHEMA.read_text())
    expected = codegen.render(doc)
    actual = codegen.OUT.read_text()
    assert actual == expected, "src/drsg/_generated.py is stale — run `python codegen.py`"
