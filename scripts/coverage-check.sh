#!/bin/sh
# Enforce exact per-file coverage counts inside the actual source root, including test modules.
# cargo-llvm-cov must export with --no-default-ignore-filename-regex; do not filter its input.
# Usage: coverage-check.sh <llvm-cov.json> [source-directory]
set -eu
report="${1:?usage: coverage-check.sh <llvm-cov.json> [source-directory]}"
source="${2:-src}"
logical=$(cd "$source" && pwd -L)
physical=$(cd "$source" && pwd -P)
jq -ers --arg logical "$logical/" --arg physical "$physical/" '
    # LLVM preserves include paths such as tests/../benches/support.rs.
    def normalized_path:
        split("/") | reduce .[] as $part ([];
            if $part == "" or $part == "." then .
            elif $part == ".." then .[0:-1]
            else . + [$part] end) | "/" + join("/");
    def metric:
        type == "object" and
        ([.count, .covered] | all(type == "number" and . >= 0 and
            . <= 9007199254740991 and . == floor)) and
        .covered <= .count and
        (if has("notcovered") then .notcovered == (.count - .covered) else true end);
    if length != 1 then error("expected exactly one coverage report") else .[0] end |
    if .type != "llvm.coverage.json.export" or
       (.version | type != "string") or
       (.version | test("^[0-9]+[.][0-9]+[.][0-9]+$") | not) or
       (.data | type != "array" or length != 1) or
       (.data[0].files | type != "array")
    then error("invalid coverage report schema") else . end |
    .data[0].files |
    if all(.[]; type == "object" and
        (.filename | type == "string" and startswith("/"))) | not
    then error("invalid coverage filename") else . end |
    [.[] | (.filename | normalized_path) as $name |
        if $name | startswith($physical) then . + {source: $name | ltrimstr($physical)}
        elif $name | startswith($logical) then . + {source: $name | ltrimstr($logical)}
        else empty end] |
    if length == 0 then error("no instrumented source files") else . end |
    if (map(.source) | unique | length) != length
    then error("duplicate source coverage entries") else . end |
    map(. as $file |
        if [.summary.lines, .summary.functions, .summary.regions] | all(metric) | not
        then error("invalid coverage metrics: " + $file.source)
        elif [.summary.lines, .summary.functions, .summary.regions] | all(.covered == .count) | not
        then error("incomplete source coverage: " + $file.source)
        else . end) |
    if [(map(.summary.lines.count) | add), (map(.summary.functions.count) | add),
        (map(.summary.regions.count) | add)] | all(. > 0) | not
    then error("source inventory contains no executable coverage") else . end |
    (.[] | ["src/" + .source, .summary.lines.count, .summary.functions.count, .summary.regions.count] | @tsv),
    (["TOTAL (100% lines/functions/regions)", length,
        (map(.summary.lines.count) | add), (map(.summary.functions.count) | add),
        (map(.summary.regions.count) | add)] | @tsv)
' "$report"
