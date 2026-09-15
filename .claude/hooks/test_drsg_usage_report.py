"""The usage-report hook's watermark handling, as a `unittest` module.

Run from the repository root: `python3 -m unittest discover -s .claude/hooks`
(`just gate-hooks`, and CI's `hooks` job). What is pinned here is the part
that touches the filesystem on someone else's behalf: the watermark lives in a
directory only this user can write, is written through a file created
exclusively and 0600, a symlink planted at the temporary name is never
followed, and a machine with no writable private directory degrades to
session-only totals rather than failing the turn.
"""

from __future__ import annotations

import importlib.util
import io
import json
import os
import stat
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load_module():
    spec = importlib.util.spec_from_file_location(
        "drsg_usage_report", HERE / "drsg_usage_report.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class WatermarkTests(unittest.TestCase):
    def setUp(self):
        self.report = load_module()
        self.scratch = tempfile.TemporaryDirectory(prefix="drsg-hook-test-")
        self.addCleanup(self.scratch.cleanup)
        self.base = Path(self.scratch.name)
        self.saved = {k: os.environ.get(k) for k in ("XDG_RUNTIME_DIR", "HOME")}
        self.addCleanup(self.restore_env)

    def restore_env(self):
        for key, value in self.saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value

    def transcript(self, name="abc123"):
        path = self.base / f"{name}.jsonl"
        path.write_text("", encoding="utf-8")
        return path

    def test_directory_is_private_and_the_file_is_owner_only(self):
        runtime = self.base / "runtime"
        runtime.mkdir()
        os.environ["XDG_RUNTIME_DIR"] = str(runtime)
        mark = self.report.watermark_path(self.transcript())
        self.assertEqual(mark.parent, runtime / "drsg")
        self.assertEqual(stat.S_IMODE(mark.parent.stat().st_mode), 0o700)
        self.report.write_watermark(mark, 42)
        self.assertEqual(stat.S_IMODE(mark.stat().st_mode), 0o600)
        self.assertEqual(json.loads(mark.read_text()), {"lines": 42})
        # No temporary left behind: the write was rename-into-place.
        self.assertEqual(sorted(p.name for p in mark.parent.iterdir()), [mark.name])

    def test_a_planted_symlink_at_the_temporary_name_is_not_followed(self):
        runtime = self.base / "runtime"
        runtime.mkdir()
        os.environ["XDG_RUNTIME_DIR"] = str(runtime)
        mark = self.report.watermark_path(self.transcript())
        victim = self.base / "victim"
        victim.write_text("precious", encoding="utf-8")
        tmp = mark.with_suffix(".tmp")
        os.symlink(victim, tmp)
        self.report.write_watermark(mark, 7)
        # The link was discarded, not written through; the mark is a plain
        # file of our own.
        self.assertEqual(victim.read_text(encoding="utf-8"), "precious")
        self.assertFalse(mark.is_symlink())
        self.assertEqual(json.loads(mark.read_text()), {"lines": 7})

    def test_something_that_cannot_be_removed_at_the_temporary_name_raises(self):
        runtime = self.base / "runtime"
        runtime.mkdir()
        os.environ["XDG_RUNTIME_DIR"] = str(runtime)
        mark = self.report.watermark_path(self.transcript())
        # A directory at the temporary name is neither ours nor removable by
        # unlink: the write must fail loudly rather than open anything.
        mark.with_suffix(".tmp").mkdir()
        with self.assertRaises(OSError):
            self.report.write_watermark(mark, 1)
        self.assertFalse(mark.exists())

    def test_no_private_directory_degrades_to_session_only(self):
        # No runtime dir, and a HOME that is a file: `~/.cache/drsg` cannot be
        # made, so `watermark_dir` raises — which `main` turns into "no
        # watermark", reporting the session without failing the turn.
        os.environ.pop("XDG_RUNTIME_DIR", None)
        home = self.base / "home-is-a-file"
        home.write_text("", encoding="utf-8")
        os.environ["HOME"] = str(home)
        with self.assertRaises(OSError):
            self.report.watermark_dir()

        transcript = self.transcript("sess")
        transcript.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "message": {
                        "usage": {"input_tokens": 1, "output_tokens": 1},
                        "content": [
                            {"type": "tool_use", "name": "mcp__drsg__context", "input": {}}
                        ],
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        sys.stdin = io.StringIO(json.dumps({"transcript_path": str(transcript)}))
        self.addCleanup(setattr, sys, "stdin", sys.__stdin__)
        out = io.StringIO()
        with redirect_stdout(out):
            code = self.report.main()
        self.assertEqual(code, 0)
        message = json.loads(out.getvalue())["systemMessage"]
        self.assertTrue(message.startswith("drsg MCP"), message)
        self.assertNotIn("could not", message)


if __name__ == "__main__":
    unittest.main()
