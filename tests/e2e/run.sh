#!/usr/bin/env bash
# End-to-end checks for the three 2027-Q1 code-intelligence features, run
# against the real binary, the real wasm parsers and a real git repository:
#
#   1. cross-layer edges  — Spring MVC routes to handlers, a Dubbo provider
#                           and consumer matched across a boundary, a MyBatis
#                           mapper to its SQL and the table it reads
#   2. affected           — the tests a change reaches, what `--safe` widens
#                           to, and what `--seeds` feeds a loop
#   3. --report           — the Markdown the CI templates paste into a pull
#                           request, including the empty-output contract they
#                           switch sections on
#
# Nothing here is mocked. The fixture is digested by whatever plugins the
# store holds, so a plugin release that drops an edge fails this script —
# which is the point: the unit tests cover the renderers, this covers the
# claim. It is deliberately not part of `just gate`: it needs the plugin
# store and several seconds, and the gate mirrors CI.
#
# Needs: a built drsg (DRSG_BIN, else ./target/debug/drsg) and git.
#
# The patterns below are Markdown, and Markdown spells code with backticks, so
# single quotes are load-bearing here rather than an oversight.
# shellcheck disable=SC2016
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
drsg=${DRSG_BIN:-$root/target/debug/drsg}

if [ ! -x "$drsg" ]; then
    echo "e2e: no executable drsg at $drsg — run 'cargo build -p drsg', or set DRSG_BIN" >&2
    exit 2
fi
command -v git >/dev/null 2>&1 || { echo "e2e: needs git on PATH" >&2; exit 2; }

work=$(mktemp -d "${TMPDIR:-/tmp}/drsg-e2e.XXXXXX")
trap 'rm -rf "$work"' EXIT

# drsg's own daily log defaults to ./logs: inside the fixture it would be a
# changed file of its own, and `affected` reads changed files.
export DRSG_LOG_DIR="$work/logs"
# The web tests' rule, for the same reason they have it: a token in the
# environment changes what the binary does.
unset DRSG_TOKEN

repo=$work/repo
out=$work/out
db=$work/graph.drsg
mkdir -p "$repo" "$out"
cp -R "$here/fixture/." "$repo"
cd "$repo"
git init -q -b main
git add -A
git -c user.email=fixture@example.invalid -c user.name=fixture commit -q -m "the fixture"

digest() {
    "$drsg" --db "$db" digest . --apply --code-only --no-embed --no-git --plane e2e \
        > "$out/digest.log" 2>&1 ||
        { echo "e2e: the digest failed"; sed -n '1,40p' "$out/digest.log"; exit 1; }
}
commit() { git add -A && git -c user.email=fixture@example.invalid -c user.name=fixture commit -q -m "$1"; }

checks=0
failed=0
layer_failed=0

say() { printf '\n%s\n' "$1"; }
ok() { checks=$((checks + 1)); printf '  ok    %s\n' "$1"; }
no() {
    checks=$((checks + 1))
    failed=$((failed + 1))
    printf '  FAIL  %s\n        %s\n' "$1" "$2"
    sed -n '1,24p' "$out/$3" | sed 's/^/        | /'
}

# Run a verb into a file. The plane is the same for every call here.
verb() {
    local file=$1
    shift
    if ! "$drsg" --db "$db" "$@" --plane e2e > "$out/$file" 2> "$out/$file.err"; then
        checks=$((checks + 1))
        failed=$((failed + 1))
        printf '  FAIL  `%s` exited non-zero\n' "$*"
        sed -n '1,12p' "$out/$file.err" | sed 's/^/        | /'
    fi
}

want() { # want <label> <ere> <file>
    if grep -Eq -- "$2" "$out/$3"; then ok "$1"; else no "$1" "no match for: $2" "$3"; fi
}
lack() { # lack <label> <ere> <file> — the negative half of a claim
    if grep -Eq -- "$2" "$out/$3"; then no "$1" "matched what it must not: $2" "$3"; else ok "$1"; fi
}
void() { # void <label> <file> — the empty-output contract the templates switch on
    if [ -s "$out/$1.f" ]; then no "$2" "wanted nothing at all, got $(wc -c < "$out/$1.f") byte(s)" "$1.f"; else ok "$2"; fi
}
# grep matches one line at a time, so an "A above B" claim is two matches and
# a comparison, never one pattern.
order() { # order <label> <ere above> <ere below> <file>
    local a b
    a=$(grep -nE -- "$2" "$out/$4" | head -1 | cut -d: -f1)
    b=$(grep -nE -- "$3" "$out/$4" | head -1 | cut -d: -f1)
    if [ -n "$a" ] && [ -n "$b" ] && [ "$a" -lt "$b" ]; then
        ok "$1"
    else
        no "$1" "wanted '$2' above '$3', matched lines ${a:-none} and ${b:-none}" "$4"
    fi
}

digest
printf 'e2e: %s\n' "$("$drsg" --version)"
printf '     %s\n' "$(grep -o 'applied: .*' "$out/digest.log" || echo 'digested')"

# ---------------------------------------------------------------------------
say "1. cross-layer edges — one request, three frameworks"
before=$failed

verb http.txt wire /
want "Spring MVC: a GET route reaches its handler" 'serves +GET +com\.acme\.OrderController\.byId' http.txt
want "Spring MVC: and a POST route its own" 'serves +POST +com\.acme\.OrderController\.place' http.txt
want "the path keeps its variable as a shape, not a name" 'path +/orders/\{\}' http.txt

verb dubbo.txt wire com.acme.Checkout
want "Dubbo: the consumer's ask is an address, not a method" 'path +com\.acme\.OrderService:orders:1\.0\.0' dubbo.txt
want "Dubbo: the provider serves that address" 'serves +DUBBO +com\.acme\.OrderServiceImpl' dubbo.txt
want "Dubbo: and the consumer is matched to it" 'requests +DUBBO +com\.acme\.Checkout' dubbo.txt

verb iface.txt wire com.acme.OrderService
want "the interface itself states no route — identity is on the two halves" 'states no route' iface.txt

verb mapper.txt context com.acme.OrderMapper.findById
want "MyBatis: the mapper method is implemented from outside Java" '^IMPLEMENTS' mapper.txt
want "MyBatis: by the statement in the XML beside it" 'OrderMapper\.xml#findById +Statement' mapper.txt

verb sql.txt context 'OrderMapper.xml#findById'
want "MyBatis: the statement reads the table its SQL names" 'READS' sql.txt
want "MyBatis: and the columns it selects on" 'orders\.(id|sku) +Column' sql.txt

verb cross.txt trace com.acme.OrderController.byId com.acme.OrderMapper.findById
want "HTTP handler to SQL in one trace: through the Dubbo provider" '\-> com\.acme\.OrderServiceImpl\.find' cross.txt
want "HTTP handler to SQL in one trace: and into the mapper" '\-> com\.acme\.OrderMapper\.findById' cross.txt

layer_failed=$((failed - before))

# ---------------------------------------------------------------------------
say "2. affected — the tests a change reaches"

verb clean.txt affected
want "a clean tree starts from nothing and says so" 'no changed declaration to start from' clean.txt

# Edit the body of `total`, leaving its extent alone.
python3 - <<'PY'
import pathlib
p = pathlib.Path("engine/src/lib.rs")
s = p.read_text()
old, new = "        sum += *item;", "        sum = sum + *item;"
assert s.count(old) == 1, "the fixture moved under the e2e"
p.write_text(s.replace(old, new))
PY

verb dirty.txt affected
want "an uncommitted edit is read against HEAD" 'the working tree against HEAD — 1 file' dirty.txt
want "the test one hop away is named" 'an_empty_slice_totals_zero +\[CALLS\]' dirty.txt
want "and the one two hops away, through the helper" 'a_summary_counts_and_sums +\[CALLS\]' dirty.txt
want "two, and no more" 'affected tests: 2 in 1 file' dirty.txt
lack "a test that reaches nothing changed stays out" 'a_name_is_shouted' dirty.txt
lack "and so does one whose only call is inside a macro" 'a_macro_is_the_only_caller' dirty.txt
want "a plane behind the working tree says so before it answers" 'the plane is behind this working tree' dirty.txt

verb safe.txt affected --safe
want "--safe widens to the package the change touched" 'affected tests: 5 in 1 file' safe.txt
want "which is how the macro-only test is recovered" 'a_macro_is_the_only_caller +\[in a changed package\]' safe.txt
want "and it says which are reached and which are swept in" 'widened: 2 reached a change, 3 more' safe.txt

verb seeds.txt affected --seeds
want "--seeds is one key per line and nothing else" '^engine::total$' seeds.txt
lack "no prose a loop would have to strip" 'affected tests|note:' seeds.txt

commit "edit total"
digest

verb vs.txt affected --vs HEAD~1
want "a committed change is read with --vs, as CI does" 'affected tests: 2 in 1 file' vs.txt
lack "and a plane digested at HEAD raises no staleness warning" 'the plane is behind' vs.txt

# ---------------------------------------------------------------------------
say "3. --report — what a pull request is given"

verb set.md affected --vs HEAD~1 --report
want "the set is a section a reviewer can read" '^### Tests this change reaches' set.md
want "with the count and the bound it was found under" '^\*\*2\*\* in 1 file\(s\), from 1 of 1 changed declaration' set.md
want "each file folds away" '<details><summary><code>engine/src/lib\.rs</code>' set.md
want "and each test says why it is here" '^\| `engine::tests::an_empty_slice_totals_zero` \| CALLS \|' set.md
want "the honesty note survives as a block quote" '^> note: recorded edges only' set.md

verb radius.md impact engine::total --depth 3 --report
want "a radius is a section per symbol" '^#### `engine::total`' radius.md
want "counted by distance, nearest named" '^\| depth \| reaching \| nearest \|' radius.md

verb routes.md wire com.acme.OrderController.byId --report
want "a route table carries both sides" '^\| method \| path \| serves \| asked by \|' routes.md
want "and an HTTP route is found from the handler a change named" '^\| GET \| `/orders/\{\}`' routes.md

# The empty-output contract: the templates decide whether a section exists by
# testing the file, never by matching prose.
"$drsg" --db "$db" impact com.acme.OrderController.byId --plane e2e --report > "$out/unreached.f" 2>/dev/null
void unreached "a symbol nothing reaches earns no section at all"
"$drsg" --db "$db" wire engine::total --plane e2e --report > "$out/routeless.f" 2>/dev/null
void routeless "a symbol on no route earns no section at all"
"$drsg" --db "$db" wire com.acme.OrderServiceImpl.find --plane e2e --report > "$out/method.f" 2>/dev/null
void method "a Dubbo route is stated by the type, so a method seed finds none"
git stash -q
"$drsg" --db "$db" affected --plane e2e --report > "$out/nochange.f" 2>/dev/null
void nochange "and a change naming no declaration earns no report"
git stash pop -q 2>/dev/null || true

# The loop both templates run: seeds out, a section per seed back.
"$drsg" --db "$db" affected --plane e2e --vs HEAD~1 --seeds > "$out/loop-seeds.txt"
: > "$out/loop-radius.md"
: > "$out/loop-routes.f"
while read -r seed; do
    [ -n "$seed" ] || continue
    "$drsg" --db "$db" impact "$seed" --plane e2e --depth 3 --report >> "$out/loop-radius.md" 2>/dev/null
    "$drsg" --db "$db" wire "$seed" --plane e2e --report >> "$out/loop-routes.f" 2>/dev/null
done < "$out/loop-seeds.txt"
want "the seed loop renders a radius for each changed declaration" '^#### `engine::total`' loop-radius.md
void loop-routes "and leaves the routes file empty when none is on a route"

cat "$out/set.md" "$out/loop-radius.md" > "$out/report.md"
order "the sections compose: the set's ### above the radius' ####" \
    '^### Tests this change reaches' '^#### `engine::total`' report.md

# ---------------------------------------------------------------------------
printf '\n%d check(s), %d failed\n' "$checks" "$failed"
if [ "$layer_failed" -gt 0 ]; then
    cat >&2 <<'HINT'

The cross-layer section is the one that depends on plugin releases. If it is
the only one failing, the store may hold an older build than the catalog pins:
  drsg plugin install java xml
HINT
fi
[ "$failed" -eq 0 ] || exit 1
