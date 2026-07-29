#!/usr/bin/env bash
# Start a real `drsg serve`, run the Zig e2e test against it, tear down.
# Skips (exit 0) if no drsg binary is found.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
zig_dir="$(cd "$here/.." && pwd)"
root="$zig_dir/../.."

find_binary() {
    if [[ -n "${DRSG_BIN:-}" ]]; then
        [[ -x "$DRSG_BIN" ]] && echo "$DRSG_BIN" || true
        return
    fi
    for profile in debug release; do
        local cand="$root/target/$profile/drsg"
        [[ -x "$cand" ]] && { echo "$cand"; return; }
    done
}

bin="$(find_binary)"
if [[ -z "$bin" ]]; then
    echo "SKIP: drsg binary not found; run 'cargo build -p dr-strange-cli'"
    exit 0
fi

port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
tmp="$(mktemp -d)"
export DRSG_TOKEN="test-token"
export DRSG_BASE_URL="http://127.0.0.1:$port"

"$bin" --db "$tmp/sdk-test.drsg" serve --addr "127.0.0.1:$port" >/dev/null 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null || true; rm -rf "$tmp"' EXIT

for _ in $(seq 1 100); do
    if python3 -c "import socket,sys; s=socket.socket(); sys.exit(0 if s.connect_ex(('127.0.0.1',$port))==0 else 1)"; then
        break
    fi
    sleep 0.05
done

cd "$zig_dir"
zig build test
echo "Zig e2e passed"
