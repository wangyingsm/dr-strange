#!/bin/sh
# Dr Strange installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin drsg-mcp
#
# Downloads a released archive from GitHub, verifies its SHA-256, and installs
# the binary. Nothing is compiled; no toolchain is required.
#
# Options (each has an environment-variable equivalent):
#   --bin <drsg|drsg-mcp|all>   binary to install         (DRSG_INSTALL_BIN,  default drsg)
#   --version <vX.Y.Z|latest>   release to install        (DRSG_VERSION,      default latest)
#   --dir <path>                installation directory    (DRSG_INSTALL_DIR,  default ~/.local/bin)
set -eu

REPO=wangyingsm/dr-strange
BIN=${DRSG_INSTALL_BIN:-drsg}
VERSION=${DRSG_VERSION:-latest}
INSTALL_DIR=${DRSG_INSTALL_DIR:-}

info() { printf '%s\n' "$*" >&2; }
err() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }

usage() {
    # Spelled out rather than read back from "$0": when the script is piped to
    # `sh` there is no file to read.
    cat <<'EOF'
Dr Strange installer for Linux and macOS.

  --bin <drsg|drsg-mcp|all>   binary to install       (DRSG_INSTALL_BIN, default drsg)
  --version <vX.Y.Z|latest>   release to install      (DRSG_VERSION,     default latest)
  --dir <path>                installation directory  (DRSG_INSTALL_DIR, default ~/.local/bin)
EOF
    exit 0
}

while [ $# -gt 0 ]; do
    case $1 in
        --bin) BIN=${2:?--bin needs a value} && shift 2 ;;
        --version) VERSION=${2:?--version needs a value} && shift 2 ;;
        --dir) INSTALL_DIR=${2:?--dir needs a value} && shift 2 ;;
        -h | --help) usage ;;
        *) err "unknown option: $1 (try --help)" ;;
    esac
done

case $BIN in
    drsg | drsg-mcp) BINS=$BIN ;;
    all) BINS='drsg drsg-mcp' ;;
    *) err "unknown binary: $BIN (expected drsg, drsg-mcp, or all)" ;;
esac

have curl || have wget || err 'neither curl nor wget is available'
have tar || err 'tar is required to unpack the release archive'

# fetch <url> <destination>; "-" writes to stdout.
fetch() {
    if have curl; then
        curl -fsSL "$1" -o "$2"
    else
        wget -qO "$2" "$1"
    fi
}

# --- target triple ----------------------------------------------------------
os=$(uname -s)
arch=$(uname -m)
case "$os $arch" in
    'Linux x86_64') target=x86_64-unknown-linux-gnu ;;
    'Linux aarch64' | 'Linux arm64') target=aarch64-unknown-linux-gnu ;;
    'Darwin arm64') target=aarch64-apple-darwin ;;
    'Darwin x86_64') target=x86_64-apple-darwin ;;
    *) err "unsupported platform: $os $arch — build from source instead (https://github.com/$REPO)" ;;
esac

# --- release version --------------------------------------------------------
if [ "$VERSION" = latest ]; then
    # Resolve through the /releases/latest redirect rather than the API, which
    # is rate-limited for unauthenticated callers.
    if have curl; then
        located=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
            "https://github.com/$REPO/releases/latest")
    else
        located=$(wget -qS --spider "https://github.com/$REPO/releases/latest" 2>&1 |
            sed -n 's/^[[:space:]]*Location:[[:space:]]*\([^[:space:]]*\).*/\1/p' | tail -n 1)
    fi
    VERSION=${located##*/}
    case $VERSION in
        v*) ;;
        *) err "could not determine the latest release; pass --version vX.Y.Z" ;;
    esac
fi

archive="dr-strange-$VERSION-$target.tar.gz"
base="https://github.com/$REPO/releases/download/$VERSION"

# --- install directory ------------------------------------------------------
if [ -z "$INSTALL_DIR" ]; then
    INSTALL_DIR=$HOME/.local/bin
fi
mkdir -p "$INSTALL_DIR" || err "cannot create $INSTALL_DIR"
[ -w "$INSTALL_DIR" ] || err "$INSTALL_DIR is not writable — pass --dir <path>"

# --- download, verify, unpack ----------------------------------------------
tmp=$(mktemp -d "${TMPDIR:-/tmp}/drsg-install.XXXXXX") || err 'cannot create a temporary directory'
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

info "Dr Strange $VERSION ($target)"
info "  downloading $archive"
fetch "$base/$archive" "$tmp/$archive" ||
    err "download failed — $VERSION may not ship an asset for $target: $base/$archive"

if fetch "$base/$archive.sha256" "$tmp/$archive.sha256" 2>/dev/null; then
    expected=$(cut -d' ' -f1 <"$tmp/$archive.sha256")
    if have sha256sum; then
        actual=$(sha256sum "$tmp/$archive" | cut -d' ' -f1)
    elif have shasum; then
        actual=$(shasum -a 256 "$tmp/$archive" | cut -d' ' -f1)
    else
        actual=
        info '  no sha256sum/shasum available; skipping checksum verification'
    fi
    if [ -n "$actual" ]; then
        [ "$actual" = "$expected" ] || err "checksum mismatch for $archive"
        info '  checksum verified'
    fi
fi

tar -xzf "$tmp/$archive" -C "$tmp" || err "cannot unpack $archive"

for b in $BINS; do
    src=$(find "$tmp" -type f -name "$b" -print | head -n 1)
    [ -n "$src" ] || err "$b is not present in $archive"
    install -m 755 "$src" "$INSTALL_DIR/$b" 2>/dev/null ||
        { cp "$src" "$INSTALL_DIR/$b" && chmod 755 "$INSTALL_DIR/$b"; } ||
        err "cannot install $b into $INSTALL_DIR"
    info "  installed $INSTALL_DIR/$b"
done

# --- PATH advice ------------------------------------------------------------
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        info ''
        info "$INSTALL_DIR is not on your PATH. Add it to your shell profile:"
        info "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

for b in $BINS; do
    case $b in
        drsg) info "Run: drsg --db graph.drsg serve" ;;
        drsg-mcp) info "Run: drsg-mcp --db /path/to/graph.drsg  (normally launched by an MCP host; no argument in a repository prepared by drsg init)" ;;
    esac
done
