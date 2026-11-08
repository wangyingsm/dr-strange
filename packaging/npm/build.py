#!/usr/bin/env python3
"""Lay out the npm packages for a release, ready to publish.

`npx @yingge/drsg` is the install path developers already have, and the one
the official MCP registry wants to see. The binary reaches them the way
esbuild's does: one wrapper package with an `optionalDependencies` entry per
platform, each holding a single prebuilt binary and declaring its `os`/`cpu`.
npm installs only the one that matches.

Why not a postinstall download — the obvious alternative — for two reasons.
It does not run under `npm ci --ignore-scripts`, which is how a lot of CI
installs; and it would fetch from GitHub at install time, which is the slow
path this project already works around in `scripts/install.sh`. Shipping the
binary inside the package instead puts it on npm's CDN, which mainland
mirrors (npmmirror) replicate — the same reason the mirror flag exists, solved
once rather than per user.

Usage:

    python3 packaging/npm/build.py 2.21.0 [--out DIR] [--from DIR]

Verifies each archive against the published SHA-256 and writes one directory
per package. `--from` takes a directory of archives already on disk — what CI
has in hand after building them, and the way to run this on a network where
pulling 100 MB back out of GitHub is the slow part:

    gh release download v2.21.0 --dir /tmp/rel
    python3 packaging/npm/build.py 2.21.0 --from /tmp/rel

It publishes nothing: run `npm publish` in each platform directory first, then
in `drsg/`, so the wrapper never points at versions that do not exist yet.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path

REPO = "wangyingsm/dr-strange"
ROOT = Path(__file__).resolve().parent

# npm refuses the bare name `drsg` as too similar to the existing `arg`, so
# the wrapper is scoped. The five platform names below were accepted as they
# are, and the installed command stays `drsg` either way.
WRAPPER = "@yingge/drsg"

# The release's target triples, and the `os`/`cpu` npm selects them by.
PLATFORMS = [
    ("x86_64-unknown-linux-gnu", "linux-x64", "linux", "x64", "drsg"),
    ("aarch64-unknown-linux-gnu", "linux-arm64", "linux", "arm64", "drsg"),
    ("x86_64-apple-darwin", "darwin-x64", "darwin", "x64", "drsg"),
    ("aarch64-apple-darwin", "darwin-arm64", "darwin", "arm64", "drsg"),
    ("x86_64-pc-windows-msvc", "win32-x64", "win32", "x64", "drsg.exe"),
]

DESCRIPTION = (
    "A code-intelligence graph for coding agents: who calls this, "
    "what a change reaches, in one round trip."
)


def fetch(url: str) -> bytes:
    """One release file. The timeout is explicit because urllib has none by
    default, and GitHub is slow enough from some networks to hang for
    minutes before anything says so."""
    with urllib.request.urlopen(url, timeout=120) as r:
        return r.read()


def archive(version: str, triple: str) -> tuple[str, str]:
    ext = "zip" if "windows" in triple else "tar.gz"
    return f"dr-strange-v{version}-{triple}.{ext}", ext


def verified(version: str, triple: str, tmp: Path, local: Path | None) -> Path:
    """One archive, checked against the SHA-256 published beside it.

    The check runs whether the bytes came off the network or out of `--from`:
    a local directory is a convenience, not a reason to trust its contents.
    """
    name, _ = archive(version, triple)
    if local is not None:
        blob = (local / name).read_bytes()
        want = (local / f"{name}.sha256").read_text().split()[0]
    else:
        base = f"https://github.com/{REPO}/releases/download/v{version}"
        blob = fetch(f"{base}/{name}")
        want = fetch(f"{base}/{name}.sha256").decode().split()[0]
    got = hashlib.sha256(blob).hexdigest()
    if got != want:
        raise SystemExit(f"{name}: sha256 {got} != published {want}")
    path = tmp / name
    path.write_bytes(blob)
    return path


def extract(path: Path, binary: str, into: Path) -> None:
    into.mkdir(parents=True, exist_ok=True)
    if path.name.endswith(".zip"):
        with zipfile.ZipFile(path) as z:
            member = next(n for n in z.namelist() if n.endswith(binary))
            (into / binary).write_bytes(z.read(member))
    else:
        with tarfile.open(path) as t:
            member = next(m for m in t.getmembers() if m.name.endswith(binary))
            src = t.extractfile(member)
            if src is None:
                raise SystemExit(f"{path.name}: {binary} is not a file")
            (into / binary).write_bytes(src.read())
    (into / binary).chmod(0o755)


def write_platform(out: Path, version: str, triple: str, slug: str,
                   os_: str, cpu: str, binary: str, tmp: Path,
                   local: Path | None) -> str:
    pkg = f"drsg-{slug}"
    d = out / pkg
    if d.exists():
        shutil.rmtree(d)
    (d / "bin").mkdir(parents=True)
    extract(verified(version, triple, tmp, local), binary, d / "bin")
    (d / "package.json").write_text(json.dumps({
        "name": pkg,
        "version": version,
        "description": f"{DESCRIPTION} Prebuilt binary for {os_} {cpu}.",
        "license": "MIT OR Apache-2.0",
        "repository": {"type": "git", "url": f"git+https://github.com/{REPO}.git"},
        "os": [os_],
        "cpu": [cpu],
        "files": ["bin"],
        "preferUnplugged": True,
    }, indent=2) + "\n")
    (d / "README.md").write_text(
        f"# {pkg}\n\nThe `{os_} {cpu}` binary for [drsg](https://github.com/{REPO}).\n"
        f"Installed automatically as an optional dependency of `drsg`; there is\n"
        f"no reason to depend on it directly.\n"
    )
    return pkg


def write_wrapper(out: Path, version: str, packages: list[str]) -> None:
    d = out / WRAPPER.rpartition("/")[2]
    if d.exists():
        shutil.rmtree(d)
    (d / "bin").mkdir(parents=True)
    (d / "package.json").write_text(json.dumps({
        "name": WRAPPER,
        "version": version,
        # Matched against server.json's `name` to verify npm ownership.
        "mcpName": f"io.github.{REPO}",
        "description": DESCRIPTION,
        "license": "MIT OR Apache-2.0",
        "repository": {"type": "git", "url": f"git+https://github.com/{REPO}.git"},
        "homepage": f"https://github.com/{REPO}#readme",
        "keywords": ["mcp", "code-intelligence", "graph", "agent", "call-graph",
                     "claude", "cursor", "codex", "rust"],
        "bin": {"drsg": "bin/drsg.js"},
        "files": ["bin"],
        "engines": {"node": ">=18"},
        "optionalDependencies": {p: version for p in packages},
    }, indent=2) + "\n")
    shutil.copy(ROOT / "wrapper.js", d / "bin" / "drsg.js")
    shutil.copy(ROOT / "wrapper-README.md", d / "README.md")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("version", help="release version, without the leading v")
    ap.add_argument("--out", default=str(ROOT / "dist"))
    ap.add_argument("--from", dest="src", default=None,
                    help="directory of release archives already on disk")
    args = ap.parse_args()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    local = Path(args.src) if args.src else None

    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        names = [
            write_platform(out, args.version, triple, slug, os_, cpu, binary, tmp, local)
            for triple, slug, os_, cpu, binary in PLATFORMS
        ]
    write_wrapper(out, args.version, names)

    wrapper = WRAPPER.rpartition("/")[2]
    print(f"npm packages for {args.version} in {out}:")
    for n, label in [(n, n) for n in names] + [(wrapper, WRAPPER)]:
        size = sum(f.stat().st_size for f in (out / n).rglob("*") if f.is_file())
        print(f"  {label:<20} {size / 1_048_576:6.1f} MB")
    print(f"\npublish the platform packages first, then {WRAPPER}:")
    print("  " + " && ".join(f"(cd {out}/{n} && npm publish --access public)" for n in names))
    print(f"  (cd {out}/{wrapper} && npm publish --access public)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
