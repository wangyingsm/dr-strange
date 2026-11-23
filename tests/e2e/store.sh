#!/usr/bin/env bash
# End-to-end checks for what the store says about its own vectors, parameters
# and expiries — issues #112, #113 and #114, each of which was reported from a
# real integration where the store did something defensible and said nothing.
#
#   1. #112  an index's width is reported, a query that contradicts it is
#            refused by name, and a bare array of numbers is promoted where a
#            declared index names the property
#   2. #113  a parameter no method defines is refused, and `digest.write`
#            takes every spelling of a label
#   3. #114  a node past its `$expires_at` is invisible to every read, with
#            `k = 1` and the expired entry nearest
#
# Driven over HTTP against a real `drsg serve`, because that is the surface all
# three were reported on: the in-process tests beside the code cannot see a
# serde attribute that never reaches the wire, nor a reply field dropped on the
# way out. Separate from `run.sh`, which digests a code fixture — this one
# needs no plugins and no git, only the binary.
#
# Needs: a built drsg (DRSG_BIN, else ./target/debug/drsg), curl and python3.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
drsg=${DRSG_BIN:-$root/target/debug/drsg}

if [ ! -x "$drsg" ]; then
    echo "e2e: no executable drsg at $drsg — run 'cargo build -p drsg', or set DRSG_BIN" >&2
    exit 2
fi
command -v curl >/dev/null 2>&1 || { echo "e2e: needs curl on PATH" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "e2e: needs python3 on PATH" >&2; exit 2; }

work=$(mktemp -d "${TMPDIR:-/tmp}/drsg-e2e-store.XXXXXX")
export DRSG_LOG_DIR="$work/logs"
# The server's own rule, not a workaround: with no token set, a loopback bind
# trusts only its same-origin browser UI, so curl is refused even for a read
# (`auth.rs`, arch/08 §4.2). Programmatic access needs a token, so this throws
# one away on a port nobody else holds — the shape the SDK suites use. Whatever
# the developer's environment holds is replaced, never read.
token=e2e-store-token
export DRSG_TOKEN=$token

port=$(python3 -c 'import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()')
url="http://127.0.0.1:$port"
server=

# Kill the server by pid, never by pattern: a `pkill -f drsg` here matches
# whatever else the developer is running, this repository's own watch server
# among it.
cleanup() {
    [ -n "$server" ] && kill "$server" 2>/dev/null && wait "$server" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

"$drsg" --db "$work/graph.drsg" serve --addr "127.0.0.1:$port" \
    > "$work/serve.log" 2>&1 &
server=$!
for _ in $(seq 100); do
    curl -fsS -o /dev/null "$url/health" 2>/dev/null && break
    kill -0 "$server" 2>/dev/null || { echo "e2e: the server exited at startup"; cat "$work/serve.log"; exit 1; }
    sleep 0.1
done
curl -fsS -o /dev/null "$url/health" 2>/dev/null || {
    echo "e2e: the server never answered /health"; cat "$work/serve.log"; exit 1
}

checks=0
failed=0
say() { printf '\n%s\n' "$1"; }
ok() { checks=$((checks + 1)); printf '  ok    %s\n' "$1"; }
no() {
    checks=$((checks + 1))
    failed=$((failed + 1))
    printf '  FAIL  %s\n        %s\n' "$1" "$2"
    # The reply is one long line with no newline of its own; without the echo
    # the next check's label is glued onto it and a filter misses the label.
    sed -n '1,8p' "$work/last.json" | sed 's/^/        | /'
    echo
}

# One JSON-RPC call. The request body is read from stdin so a heredoc can hold
# it without a quoting layer, and the reply is left in $work/last.json for the
# assertions and for a failure to print.
rpc() {
    curl -fsS -X POST -H 'content-type: application/json' \
        -H "authorization: Bearer $token" --data-binary @- \
        "$url/rpc" > "$work/last.json" 2>"$work/last.err" || {
        printf '  FAIL  the call itself failed\n'
        checks=$((checks + 1))
        failed=$((failed + 1))
        sed -n '1,8p' "$work/last.err" | sed 's/^/        | /'
        echo
    }
}

# want <label> <python expression over `r`, the parsed reply>
want() {
    if python3 -c "
import json, sys
r = json.load(open('$work/last.json'))
sys.exit(0 if ($2) else 1)
" 2>/dev/null; then ok "$1"; else no "$1" "not true of the reply: $2"; fi
}

# A 16-wide vector, the floor at which an array is read as an embedding, and a
# second one that is a worse match for the first's own query.
near=$(python3 -c 'print(repr([1.0] + [0.0] * 15))')
far=$(python3 -c 'print(repr([0.0] * 15 + [1.0]))')
# A hundred-second-wide window either side of now, in epoch milliseconds.
past=$(python3 -c 'import time; print(int(time.time() * 1000) - 100000)')
future=$(python3 -c 'import time; print(int(time.time() * 1000) + 3600000)')

printf 'e2e/store: %s on %s\n' "$("$drsg" --version)" "$url"

# ---------------------------------------------------------------------------
say "1. #112 — the width an index holds, and what contradicts it"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"index.ensure",
 "params":{"plane":"startup","label":"Doc","property":"emb","kind":"vector","metric":"l2"}}
JSON
want "a vector index is declared over HTTP" "r.get('error') is None"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.create",
 "params":{"plane":"startup","key":"near","labels":["Doc"],
           "properties":{"emb":{"\$vector":$near}}}}
JSON
want "and a node written into it" "r.get('error') is None"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"plane.indexes","params":{"plane":"startup"}}
JSON
want "plane.indexes reports the width it holds" \
    "[i['dim'] for i in r['result']['vector'] if i['property'] == 'emb'] == [16]"
want "and does not call it mixed" \
    "[i['mixed'] for i in r['result']['vector'] if i['property'] == 'emb'] == [False]"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"plane.search",
 "params":{"plane":"startup","label":"Doc","property":"emb","query":[1.0,0.0,0.0,0.0],"k":5}}
JSON
want "a query the index's width contradicts is refused" "r.get('error') is not None"
want "with invalid-params, not an application error" "r['error']['code'] == -32602"
want "and the message names both widths" \
    "'4-wide' in r['error']['message'] and '16-wide' in r['error']['message']"
want "and the pair it is talking about" "'(Doc, emb)' in r['error']['message']"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"plane.search",
 "params":{"plane":"startup","label":"Doc","property":"nothing_declared","query":[1.0,0.0],"k":5}}
JSON
want "a search on an undeclared property is not refused — the width is unknown" \
    "r.get('error') is None and r['result'] == []"

# The bare array: what the reporter's client actually sent.
rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.create",
 "params":{"plane":"startup","key":"bare","labels":["Doc"],"properties":{"emb":$far}}}
JSON
want "a bare array of numbers is accepted" "r.get('error') is None"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.get","params":{"plane":"startup","key":"bare"}}
JSON
want "and read back as a vector, not a list" \
    "'vector' in json.dumps(r['result']['properties']['emb']).lower()"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"plane.search",
 "params":{"plane":"startup","label":"Doc","property":"emb","query":$far,"k":1}}
JSON
want "so the promoted vector answers its own query" \
    "r.get('error') is None and r['result'][0]['external_key'] == 'bare'"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.create",
 "params":{"plane":"startup","key":"scores","labels":["Doc"],"properties":{"emb":[0.5,0.25]}}}
JSON
rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.get","params":{"plane":"startup","key":"scores"}}
JSON
want "a two-number array under the same index stays the list it is" \
    "r['result']['properties']['emb'] == [0.5, 0.25]"

# ---------------------------------------------------------------------------
say "2. #113 — a parameter nobody defined, and a label spelled three ways"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.create",
 "params":{"plane":"startup","key":"typo","labelz":["Doc"],"properties":{}}}
JSON
want "a misspelled parameter is refused" "r.get('error') is not None"
want "with invalid-params" "r['error']['code'] == -32602"
want "and the offending name in the message" "'labelz' in r['error']['message']"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.get","params":{"plane":"startup","key":"typo"}}
JSON
want "so the node it would have written does not exist" "r['result'] is None"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"plane.query",
 "params":{"plane":"startup","plan":{"source":{"ScanLabel":"Doc"},"steps":[]},"as_of_typo":1}}
JSON
want "the five methods that flatten the time-travel params still ignore the unknown" \
    "r.get('error') is None"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"digest.write",
 "params":{"plane":"startup","on_conflict":"update",
           "nodes":[{"key":"q-1","labels":["Question"],"properties":{}},
                    {"key":"q-2","label":"Answer","extra_labels":["Cached"],"properties":{}},
                    {"key":"q-3","properties":{}}]}}
JSON
want "digest.write takes nodes spelled three different ways" "r.get('error') is None"
want "and reports what it wrote" "r['result']['nodes_written'] == 3"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.get","params":{"plane":"startup","key":"q-1"}}
JSON
want "a node given 'labels' lands under them, not under Entity" \
    "r['result']['labels'] == ['Question']"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.get","params":{"plane":"startup","key":"q-2"}}
JSON
want "'label' and 'extra_labels' are unioned rather than one dropped" \
    "sorted(r['result']['labels']) == ['Answer', 'Cached']"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.get","params":{"plane":"startup","key":"q-3"}}
JSON
want "and a node naming none still falls back to Entity" \
    "r['result']['labels'] == ['Entity']"

# ---------------------------------------------------------------------------
say "3. #114 — an expiry the store owns"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"plane.create","params":{"name":"cache"}}
JSON
want "a plane for the cache" "r.get('error') is None"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.create",
 "params":{"plane":"cache","key":"stale","labels":["Answer"],
           "properties":{"emb":{"\$vector":$near},"\$expires_at":$past}}}
JSON
want "and an entry in it that expired a hundred seconds ago" "r.get('error') is None"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"index.ensure",
 "params":{"plane":"cache","label":"Answer","property":"emb","kind":"vector","metric":"l2"}}
JSON
rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.create",
 "params":{"plane":"cache","key":"live","labels":["Answer"],
           "properties":{"emb":{"\$vector":$far},"\$expires_at":$future}}}
JSON
want "and a live entry beside it, expiring an hour out" "r.get('error') is None"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.get","params":{"plane":"cache","key":"stale"}}
JSON
want "the expired entry is gone from node.get" "r['result'] is None"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.get","params":{"plane":"cache","key":"live"}}
JSON
want "while the one still in date is answered" "r['result']['external_key'] == 'live'"

# The reporter's exact failure: k = 1, and the expired entry is the nearest.
rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"plane.search",
 "params":{"plane":"cache","label":"Answer","property":"emb","query":$near,"k":1}}
JSON
want "a search for one neighbour answers the live entry, not the expired nearest" \
    "len(r['result']) == 1 and r['result'][0]['external_key'] == 'live'"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"plane.query",
 "params":{"plane":"cache","plan":{"source":{"ScanLabel":"Answer"},"steps":[]}}}
JSON
want "and a scan of the plane sees one node where two were written" \
    "[n['external_key'] for n in r['result']] == ['live']"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"plane.cypher",
 "params":{"plane":"cache","query":"MATCH (a:Answer) RETURN a"}}
JSON
want "Cypher does not see it either, and does see the live one" \
    "'live' in json.dumps(r) and 'stale' not in json.dumps(r)"

# Expiry hides a node; it does not free its key. A cache refreshing the entry
# writes the same key again and expects to land on it.
rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"digest.write",
 "params":{"plane":"cache","on_conflict":"update",
           "nodes":[{"key":"stale","labels":["Answer"],
                     "properties":{"emb":{"\$vector":$near},"\$expires_at":$future}}]}}
JSON
want "rewriting the expired key succeeds" "r.get('error') is None"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"node.get","params":{"plane":"cache","key":"stale"}}
JSON
want "and the entry is live again on the same row" "r['result']['external_key'] == 'stale'"

rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"plane.search",
 "params":{"plane":"cache","label":"Answer","property":"emb","query":$near,"k":1}}
JSON
want "so the refreshed entry is the nearest once more" \
    "r['result'][0]['external_key'] == 'stale'"

# Nothing swept the plane: the expired node was never deleted, only hidden,
# and the plane the writes went through is the one that reads filter.
rpc <<JSON
{"jsonrpc":"2.0","id":1,"method":"plane.list","params":{}}
JSON
want "the cache plane is one the server lists like any other" \
    "any(p['name'] == 'cache' for p in r['result'])"

# ---------------------------------------------------------------------------
printf '\n%d check(s), %d failed\n' "$checks" "$failed"
[ "$failed" -eq 0 ] || exit 1
