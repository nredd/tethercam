#!/bin/sh
# Self-test for `coverage-pending.sh`, run by `make coverage`.
set -eu

script="$(cd "$(dirname "$0")" && pwd)/coverage-pending.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
git init -q .
git config user.email test@example.invalid
git config user.name test
mkdir src
touch src/a.rs src/b.rs src/c.rs

fail() { echo "test-coverage-pending: $1" >&2; exit 1; }
expect_fail() { if "$@" 2>/dev/null; then fail "expected failure: $*"; fi; }

# No list: nothing ignored, nothing to validate, shrink check is a no-op.
[ -z "$("$script" regex)" ] || fail "regex without a list"
"$script" validate
git add -A && git commit -qm base
"$script" shrink HEAD

# The introducing change may list anything: the base has no list yet.
printf '# transition list\nsrc/a.rs #1\n\nsrc/b.rs #2\n' >coverage-pending.txt
"$script" validate
"$script" shrink HEAD
[ "$("$script" regex)" = '/(src/a\.rs|src/b\.rs)$' ] || fail "regex: $("$script" regex)"

# Later changes may shrink the list but not grow it.
git add -A && git commit -qm list
printf 'src/a.rs #1\n' >coverage-pending.txt
"$script" shrink HEAD
printf 'src/a.rs #1\nsrc/b.rs #2\nsrc/c.rs #3\n' >coverage-pending.txt
expect_fail "$script" shrink HEAD
rm coverage-pending.txt
"$script" shrink HEAD
[ -z "$("$script" regex)" ] || fail "regex after removal"

# Validation rejects missing files, non-src paths and missing refs.
printf 'src/missing.rs #1\n' >coverage-pending.txt
expect_fail "$script" validate
printf 'build.rs #1\n' >coverage-pending.txt
expect_fail "$script" validate
printf 'src/a.rs\n' >coverage-pending.txt
expect_fail "$script" validate
printf 'src/a.rs #1 extra\n' >coverage-pending.txt
expect_fail "$script" validate
# A last line without a trailing newline is still validated.
printf 'src/a.rs #1\nsrc/missing.rs #2' >coverage-pending.txt
expect_fail "$script" validate
printf 'src/a.rs #1\nsrc/b.rs #2' >coverage-pending.txt
"$script" validate

# Tracking refs must be `#<digits>`.
for ref in '#TBD' '#' '42' '#1a' '#-1'; do
    printf 'src/a.rs %s\n' "$ref" >coverage-pending.txt
    expect_fail "$script" validate
done

# Paths are literal: metacharacters, `..` and odd components are rejected.
for bad in 'src/[a-z]*.rs' 'src/a.*' 'src/../a.rs' 'src//a.rs' 'src/.rs' 'src/a/.rs' 'src/a?.rs' 'src/a|b.rs' 'src/a b.rs' '/src/a.rs' 'src/a.rs/' 'src/a.txt'; do
    printf '%s #1\n' "$bad" >coverage-pending.txt
    expect_fail "$script" validate
done

# The regex matches exactly the listed paths, as llvm-cov sees them (absolute).
mkdir -p src/x
touch src/ab.rs src/a_rs src/x/a.rs src/a-b.rs
printf 'src/a.rs #1\nsrc/a-b.rs #2\n' >coverage-pending.txt
"$script" validate
re=$("$script" regex)
matches() { printf '%s\n' "$1" | grep -Eq "$re"; }
matches /repo/src/a.rs || fail "regex misses src/a.rs"
matches /repo/src/a-b.rs || fail "regex misses src/a-b.rs"
for other in /repo/src/ab.rs /repo/src/a_rs /repo/src/x/a.rs /repo/src/main.rs /repo/src/a.rs.bak /repo/xsrc/a.rs; do
    if matches "$other"; then fail "regex wrongly matches $other"; fi
done
# An empty (comments only) list yields no regex at all, so no flag is passed.
printf '# nothing pending\n' >coverage-pending.txt
[ -z "$("$script" regex)" ] || fail "empty list produced a regex"

# Check each metric independently and fail closed on missing or malformed reports.
printf 'src/a.rs #1\n' >coverage-pending.txt
printf '%s\n' '{"data":[{"files":[{"filename":"/repo/src/a.rs","summary":{"lines":{"count":10,"covered":10},"functions":{"count":2,"covered":2},"regions":{"count":5,"covered":5}}}]}]}' >report.json
if "$script" stale report.json 2>error; then fail "fully covered file was accepted"; fi
grep -q 'remove these entries' error || fail "missing stale diagnostic"
grep -q 'src/a.rs' error || fail "missing stale path"
for metric in lines functions regions; do
    jq --arg metric "$metric" '.data[0].files[0].summary[$metric].covered -= 1' report.json >partial.json
    "$script" stale partial.json || fail "$metric below 100% was stale"
done
printf 'src/b.rs #2\n' >coverage-pending.txt
expect_fail "$script" stale report.json
expect_fail "$script" stale missing.json
printf 'not json\n' >bad.json
expect_fail "$script" stale bad.json
: >bad.json
expect_fail "$script" stale bad.json
printf '# empty\n' >coverage-pending.txt
"$script" stale report.json
expect_fail "$script" stale bad.json

# An unknown base skips the shrink check instead of failing.
"$script" shrink no-such-ref
expect_fail "$script" bogus
echo "test-coverage-pending: ok"
