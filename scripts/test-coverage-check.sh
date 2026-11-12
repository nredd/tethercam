#!/bin/sh
# Negative fixtures for the complete-source gate. No tool defaults or rounded percentages.
set -eu
script=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)/coverage-check.sh
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
mkdir -p "$tmp/real/src"
ln -s "$tmp/real" "$tmp/logical"
physical=$(cd "$tmp/real/src" && pwd -P)
logical="$tmp/logical/src"
# Reports are synthetic; real coverage is separately collected by `make coverage`.
jq -n --arg root "$physical" '
    {type: "llvm.coverage.json.export", version: "3.1.0", data: [{files: [
        {filename: ($root + "/module.rs"), summary: {
            lines: {count: 1, covered: 1}, functions: {count: 1, covered: 1},
            regions: {count: 1, covered: 1}}}]}]}
' > "$tmp/good.json"
cases=0
pass() {
    cases=$((cases + 1))
    "$script" "$tmp/case.json" "$logical" > "$tmp/out" 2>&1 || {
        printf 'coverage fixture unexpectedly rejected: %s\n' "$1" >&2
        cat "$tmp/out" >&2
        exit 1
    }
}
fail() {
    cases=$((cases + 1))
    if "$script" "$tmp/case.json" "$logical" > "$tmp/out" 2>&1; then
        printf 'coverage fixture unexpectedly accepted: %s\n' "$1" >&2
        exit 1
    fi
}
cp "$tmp/good.json" "$tmp/case.json"
pass physical-root
jq --arg root "$logical" '.data[0].files[0].filename = ($root + "/module.rs")' "$tmp/good.json" > "$tmp/case.json"
pass logical-root
jq '.data[0].files[0].filename |= sub("module.rs$"; "nested/.././tests.rs")' "$tmp/good.json" > "$tmp/case.json"
pass normalized-source-path
jq '.data[0].files[0].filename |= sub("module.rs$"; "nested/.././tests.rs") | .data[0].files[0].summary.lines.covered = 0' "$tmp/good.json" > "$tmp/case.json"
fail normalized-source-miss
jq '.data[0].files += [(.data[0].files[0] | .filename |= sub("module.rs$"; "../tests/../benches/support.rs") | .summary.lines.covered = 0)]' "$tmp/good.json" > "$tmp/case.json"
pass normalized-outside-path
for name in tests.rs foo_tests.rs nested/tests/module.rs; do
    for metric in lines functions regions; do
        jq --arg root "$physical" --arg name "$name" --arg metric "$metric" '
            .data[0].files[0].filename = ($root + "/" + $name) |
            .data[0].files[0].summary[$metric] = {count: 1000000, covered: 999999, percent: 100}
        ' "$tmp/good.json" > "$tmp/case.json"
        fail "$name/$metric rounded miss"
    done
done
for name in tests/helper.rs benches/helper.rs examples/helper.rs src-other/helper.rs dependency/src/tests.rs; do
    jq --arg root "$physical" --arg name "$name" '
        .data[0].files += [(.data[0].files[0] |
            .filename = ($root + "/../" + $name) | .summary.regions.covered = 0)] |
        .data[0].files[1].filename |= sub("/src/../"; "/")
    ' "$tmp/good.json" > "$tmp/case.json"
    pass "outside $name"
done
for filter in \
    'null' '{}' '.type = "wrong"' '.version = null' '.version = "unknown"' \
    '.data = null' '.data = []' '.data += .data' '.data[0].files = null' \
    '.data[0].files = []' '.data[0].files[0].filename = null' \
    '.data[0].files[0].filename = "src/tests.rs"' \
    '.data[0].files[0].filename = ""' \
    '.data[0].files += .data[0].files' \
    'del(.data[0].files[0].summary)' \
    '.data[0].files[0].summary.lines = null' \
    '.data[0].files[0].summary.lines.count = -1' \
    '.data[0].files[0].summary.lines.count = 1.5' \
    '.data[0].files[0].summary.lines.covered = "1"' \
    '.data[0].files[0].summary.lines.covered = 2' \
    '.data[0].files[0].summary.lines.covered = -1' \
    '.data[0].files[0].summary.lines.covered = null' \
    '.data[0].files[0].summary.regions.notcovered = 1' \
    '.data[0].files[0].summary.functions = {}' \
    '.data[0].files[0].summary.regions.covered = 0.5' \
    '.data[0].files[0].summary |= map_values({count: 0, covered: 0})' \
    '.data[0].files[0].filename = "/outside/src/tests.rs"'; do
    jq "$filter" "$tmp/good.json" > "$tmp/case.json"
    fail "$filter"
done
# Both root aliases identifying one source file must not inflate the inventory.
jq --arg root "$logical" '.data[0].files += [(.data[0].files[0] | .filename = ($root + "/module.rs"))]' "$tmp/good.json" > "$tmp/case.json"
fail duplicate-alias
cp "$tmp/good.json" "$tmp/case.json"
cat "$tmp/good.json" >> "$tmp/case.json"
fail multiple-json-documents
printf '{bad json' > "$tmp/case.json"
fail malformed-json
: > "$tmp/case.json"
fail empty-file
rm "$tmp/case.json"
fail missing-file
# Legitimately non-executable files do not constitute uncovered executable code.
jq '.data[0].files += [(.data[0].files[0] | .filename += "-empty" | .summary |= map_values({count: 0, covered: 0}))]' "$tmp/good.json" > "$tmp/case.json"
pass non-executable-file
# Only the exact thin adapter can be locally ungated, never a test module or sibling.
jq '.data[0].files += [(.data[0].files[0] |
    .filename |= sub("module.rs$"; "transport/usb/nusb_backend.rs") |
    .summary.lines.covered = 0)]' "$tmp/good.json" > "$tmp/case.json"
CI= fail ci-empty-no-exception
CI=false fail ci-false-no-exception
CI=0 fail ci-zero-no-exception
CI=1 fail ci-true-no-exception
if test ! -d /sys/devices/platform/vhci_hcd.0; then
    cases=$((cases + 1))
    (unset CI; "$script" "$tmp/case.json" "$logical") > "$tmp/out" 2>&1
    grep -q 'LOCAL ONLY: src/transport/usb/nusb_backend.rs' "$tmp/out"
fi
jq '.data[0].files[1].summary.lines.covered = "invalid"' "$tmp/case.json" > "$tmp/invalid.json"
cp "$tmp/invalid.json" "$tmp/case.json"
fail local-exception-still-validates-schema
for name in nusb_backend_tests.rs nusb_backend.rs/child.rs nusb_backend.rs.bak; do
    jq --arg name "$name" '.data[0].files += [(.data[0].files[0] |
        .filename |= sub("module.rs$"; "transport/usb/" + $name) |
        .summary.lines.covered = 0)]' "$tmp/good.json" > "$tmp/case.json"
    fail "$name not eligible"
done
printf 'coverage-check: %s fixtures passed\n' "$cases"
