#!/usr/bin/env bash
# Start a real `drsg serve`, run the compiled e2e binary against it, tear down.
# Skips (exit 0) if no drsg binary is found.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../../.."

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

# A misbehaving WebSocket peer for the client's hostile-input checks.
fake_port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
export DRSG_FAKE_URL="http://127.0.0.1:$fake_port"
python3 "$here/fake_ws.py" "$fake_port" &
fake=$!
# Wait for the server to exit before removing its directory: a dying
# `drsg serve` may still be flushing files there, and rm -rf racing it
# fails with "Directory not empty" and turns a green run red.
cleanup() {
    kill "$server" "$fake" 2>/dev/null || true
    wait "$server" "$fake" 2>/dev/null || true
    rm -rf "$tmp"
}
trap cleanup EXIT

for p in "$port" "$fake_port"; do
    for _ in $(seq 1 100); do
        if python3 -c "import socket,sys; s=socket.socket(); sys.exit(0 if s.connect_ex(('127.0.0.1',$p))==0 else 1)"; then
            break
        fi
        sleep 0.05
    done
done

"$here/e2e"
