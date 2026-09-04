#!/usr/bin/env python3
"""Summarise a session's drsg MCP usage for the Stop hook.

What is exact and what is not, said out loud because a metric nobody trusts is
worse than none:

* **Call counts** are exact — one per `tool_use` block naming an `mcp__drsg*`
  tool.
* **Bytes returned** are exact — the length of each matching `tool_result`.
* **Tokens returned** are an *estimate* from those bytes (~4 chars/token) and
  are always printed with a `~`. The API reports usage per request, not per
  content block, so no exact per-tool figure exists to read.
* **Model I/O** is exact — summed from each assistant message's own `usage`.

"This turn" is the delta since the last report, tracked by remembering how many
transcript lines had been read. A missing or stale watermark degrades to
"session only" rather than to a wrong number.
"""

from __future__ import annotations

import json
import os
import re
import sys
import tempfile
from pathlib import Path

# Both the stdio server (`drsg`) and the watched one (`drsg-watch`) count: they
# are the same graph reached two ways, and a session may have either attached.
TOOL_PREFIX = re.compile(r"^mcp__drsg[\w-]*__(?P<verb>.+)$")

# Characters per token. A rough divisor for English prose and JSON alike; the
# figure it produces is only ever shown with a `~`.
CHARS_PER_TOKEN = 4

# A Bash call that searched or read code the graph could have answered: the
# first word of the command line, past env assignments and an `rtk` prefix.
# Counted beside the graph calls, because the number that says whether the
# graph is earning its place is not how often it was asked but how often it
# was *not* asked when it could have been. `DRSG_RAW=1` marks a deliberate
# shell command and is not counted; neither is a write (`>`/heredoc).
SHELL_READ = re.compile(
    r"^\s*(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*(?:rtk\s+)?(?:\S*/)?"
    r"(?:rg|grep|egrep|fgrep|ugrep|ug|ag|ack|cat|head|tail|bat|less|more|sed\s+-[a-zA-Z]*n)\b"
)


def is_shell_read(command: str) -> bool:
    if "DRSG_RAW=1" in command or ">" in command or "<<" in command:
        return False
    return bool(SHELL_READ.match(command))


def read_hook_input() -> dict:
    try:
        raw = sys.stdin.read()
    except Exception:
        return {}
    try:
        return json.loads(raw) if raw.strip() else {}
    except Exception:
        return {}


def project_transcript_dir() -> Path:
    """`~/.claude/projects/<cwd with slashes turned into dashes>`."""
    root = os.environ.get("CLAUDE_PROJECT_DIR") or os.getcwd()
    slug = str(Path(root).resolve()).replace("/", "-")
    return Path.home() / ".claude" / "projects" / slug


def find_transcript(hook: dict) -> Path | None:
    """The transcript this hook is about.

    Three ways, narrowest first: what the hook was told, then the session id
    under this project's directory, then the newest transcript there. The
    fallbacks exist because the report is worth more than its own precision
    about *which* session — but a guess is only made when nothing better is on
    offer.
    """
    given = hook.get("transcript_path")
    if given and Path(given).is_file():
        return Path(given)

    directory = project_transcript_dir()
    session = hook.get("session_id")
    if session:
        candidate = directory / f"{session}.jsonl"
        if candidate.is_file():
            return candidate

    # The newest transcript is a fallback only when the hook named no session at
    # all. If it named one that cannot be found, reporting a *different*
    # session's numbers under this session's report would be worse than
    # reporting nothing — the figures would look entirely plausible.
    if given or session:
        return None
    if directory.is_dir():
        transcripts = sorted(
            directory.glob("*.jsonl"), key=lambda p: p.stat().st_mtime, reverse=True
        )
        if transcripts:
            return transcripts[0]
    return None


def watermark_path(transcript: Path) -> Path:
    # Temp rather than the repo: this is per-session bookkeeping with no meaning
    # after the session, and the working tree of a project under review is the
    # last place it should show up.
    return Path(tempfile.gettempdir()) / f"drsg-usage-{transcript.stem}.json"


def blocks(entry: dict):
    content = (entry.get("message") or {}).get("content")
    return content if isinstance(content, list) else []


def text_len(block: dict) -> int:
    """How much a tool result actually put in the context window."""
    content = block.get("content")
    if isinstance(content, str):
        return len(content)
    total = 0
    if isinstance(content, list):
        for part in content:
            if isinstance(part, dict):
                total += len(part.get("text") or "")
            elif isinstance(part, str):
                total += len(part)
    return total


class Tally:
    def __init__(self) -> None:
        self.calls: dict[str, int] = {}
        self.result_chars = 0
        # Fresh input: what the model had to be *sent*. Cache reads are counted
        # apart because they are the same context re-read on every request —
        # summing them across a long session produces a number in the hundreds
        # of millions that means nothing anyone would recognise as usage.
        self.tokens_new_in = 0
        self.tokens_cache_read = 0
        self.tokens_out = 0
        self.requests = 0
        # Shell searches and reads on code — see `is_shell_read`.
        self.shell_reads = 0

    @property
    def total_calls(self) -> int:
        return sum(self.calls.values())

    def verbs(self, limit: int = 4) -> str:
        ranked = sorted(self.calls.items(), key=lambda kv: (-kv[1], kv[0]))
        shown = ", ".join(f"{verb}×{n}" for verb, n in ranked[:limit])
        rest = len(ranked) - limit
        return f"{shown}, +{rest} more" if rest > 0 else shown


def scan(transcript: Path, since_line: int) -> tuple[Tally, Tally, int]:
    """Walk the transcript once, tallying the whole session and the new tail.

    Tool calls and their results are separate entries, so a call is attributed
    to the turn it was *made* in: the id map is kept for the whole session and a
    result lands in whichever tally claimed its id.
    """
    session, turn = Tally(), Tally()
    owner: dict[str, tuple[str, bool]] = {}  # tool_use_id -> (verb, in this turn)
    lines = 0

    with transcript.open(encoding="utf-8", errors="replace") as handle:
        for lines, raw in enumerate(handle, start=1):
            raw = raw.strip()
            if not raw:
                continue
            try:
                entry = json.loads(raw)
            except Exception:
                continue
            fresh = lines > since_line
            tallies = (session, turn) if fresh else (session,)

            usage = (entry.get("message") or {}).get("usage")
            if entry.get("type") == "assistant" and isinstance(usage, dict):
                for tally in tallies:
                    tally.requests += 1
                    tally.tokens_new_in += (usage.get("input_tokens") or 0) + (
                        usage.get("cache_creation_input_tokens") or 0
                    )
                    tally.tokens_cache_read += usage.get("cache_read_input_tokens") or 0
                    tally.tokens_out += usage.get("output_tokens") or 0

            for block in blocks(entry):
                if not isinstance(block, dict):
                    continue
                if block.get("type") == "tool_use":
                    if block.get("name") == "Bash":
                        command = (block.get("input") or {}).get("command") or ""
                        if isinstance(command, str) and is_shell_read(command):
                            for tally in tallies:
                                tally.shell_reads += 1
                        continue
                    match = TOOL_PREFIX.match(block.get("name") or "")
                    if not match:
                        continue
                    verb = match.group("verb")
                    for tally in tallies:
                        tally.calls[verb] = tally.calls.get(verb, 0) + 1
                    if block.get("id"):
                        owner[block["id"]] = (verb, fresh)
                elif block.get("type") == "tool_result":
                    known = owner.get(block.get("tool_use_id") or "")
                    if not known:
                        continue
                    size = text_len(block)
                    session.result_chars += size
                    if known[1]:
                        turn.result_chars += size
    return session, turn, lines


def human(n: int) -> str:
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}k"
    return str(n)


def approx_tokens(chars: int) -> str:
    return human(round(chars / CHARS_PER_TOKEN))


def render(session: Tally, turn: Tally, tracked: bool) -> str:
    # Without a watermark this is the session's first report, and the turn *is*
    # the session so far — printing both would print the same figure twice.
    def shell(tally: Tally) -> str:
        return f", shell search/read ×{tally.shell_reads}" if tally.shell_reads else ""

    if not tracked:
        head = None
    elif turn.total_calls:
        head = (
            f"turn: {turn.total_calls} call"
            f"{'' if turn.total_calls == 1 else 's'} ({turn.verbs()}), "
            f"~{approx_tokens(turn.result_chars)} tok returned{shell(turn)}"
        )
    else:
        head = f"turn: no calls{shell(turn)}"

    if session.total_calls:
        tail = (
            f"session: {session.total_calls} call"
            f"{'' if session.total_calls == 1 else 's'} ({session.verbs()}), "
            f"~{approx_tokens(session.result_chars)} tok returned{shell(session)}"
        )
    else:
        tail = f"session: the code graph has not been asked anything yet{shell(session)}"

    # The nudge: said only when the shell was reached for more than the
    # graph was, this turn — the one comparison that names the habit.
    scope = turn if tracked else session
    nudge = None
    if scope.shell_reads and scope.shell_reads > scope.total_calls:
        nudge = (
            "the shell was searched or read more than the graph was asked — "
            "grep/snippet/context answer these with the tree attached"
        )

    io = (
        f"model I/O {human(session.tokens_new_in)} new in, "
        f"{human(session.tokens_out)} out, "
        f"{human(session.tokens_cache_read)} cache reads over "
        f"{session.requests} request{'' if session.requests == 1 else 's'}"
    )
    parts = [p for p in (head, tail, nudge, io) if p]
    return "drsg MCP — " + " · ".join(parts)


def main() -> int:
    hook = read_hook_input()
    transcript = find_transcript(hook)
    if transcript is None:
        # Said, not swallowed: a silent reporter is indistinguishable from a
        # session that never touched the graph.
        emit("drsg MCP — no session transcript found; usage not reported")
        return 0

    mark = watermark_path(transcript)
    since = 0
    tracked = False
    try:
        state = json.loads(mark.read_text())
        if isinstance(state.get("lines"), int):
            since, tracked = state["lines"], True
    except Exception:
        pass

    try:
        session, turn, lines = scan(transcript, since)
    except Exception as exc:  # a report is never worth failing a turn over
        emit(f"drsg MCP — could not read {transcript.name}: {exc}")
        return 0

    # A transcript that shrank (a rewind, or a different session reusing the
    # name) makes the old watermark a lie; start over rather than report a
    # negative turn.
    if since > lines:
        session, turn, lines = scan(transcript, 0)
        tracked = False

    try:
        tmp = mark.with_suffix(".tmp")
        tmp.write_text(json.dumps({"lines": lines}))
        tmp.replace(mark)
    except Exception:
        pass

    emit(render(session, turn, tracked))
    return 0


def emit(message: str) -> None:
    print(json.dumps({"systemMessage": message}))


if __name__ == "__main__":
    sys.exit(main())
