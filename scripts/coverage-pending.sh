#!/bin/sh
# Helper for `coverage-pending.txt`, the transition list of `src/` files that are exempt
# from the 100% coverage gate (issue #25). The list may only shrink.
#
# Usage:
#   coverage-pending.sh regex           print an `--ignore-filename-regex` for the listed paths
#   coverage-pending.sh validate        every entry is `src/...` path that exists, with a tracking ref
#   coverage-pending.sh shrink <base>   fail if the list lists a path that <base> does not
#
# File format, one entry per line: `<path> #<issue number>`, e.g. `src/config.rs #42`.
# Blank lines and lines starting with `#` are ignored.
set -eu

LIST="${COVERAGE_PENDING_FILE:-coverage-pending.txt}"

# Print the entry paths of a list read from stdin.
paths() {
    sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*$/d' | awk '{print $1}'
}

# The list's paths, or nothing when the file is gone (the mechanism is retired).
current_paths() {
    if [ -f "$LIST" ]; then
        paths <"$LIST"
    fi
}

cmd="${1:-}"
case "$cmd" in
regex)
    # Entries are literal paths: escape every metacharacter, then anchor on a separator in front
    # of `src/` (llvm-cov matches the absolute path) and on the end. An empty list prints nothing.
    current_paths | sed -e 's/[][\\.^$*+?(){}|-]/\\&/g' | paste -sd'|' - | sed -e '/./s/^\(.*\)$/\/(\1)$/'
    ;;
validate)
    status=0
    if [ -f "$LIST" ]; then
        sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*$/d' "$LIST" | while read -r path ref extra || [ -n "$path" ]; do
            # Literal `src/` Rust paths only: no metacharacters, `..` or empty components.
            if ! printf '%s\n' "$path" | grep -Eq '^src/[A-Za-z0-9_/-]+\.rs$' ||
                printf '%s\n' "$path" | grep -Eq '//|/\.rs$'; then
                echo "$LIST: '$path' is not a plain src/ Rust file path" >&2; exit 1
            fi
            [ -f "$path" ] || { echo "$LIST: '$path' does not exist" >&2; exit 1; }
            [ -z "$extra" ] && printf '%s\n' "$ref" | grep -Eq '^#[0-9]+$' ||
                { echo "$LIST: '$path' needs exactly one tracking ref of the form #<digits>" >&2; exit 1; }
        done || status=1
    fi
    exit "$status"
    ;;
shrink)
    base="${2:?usage: coverage-pending.sh shrink <base-ref>}"
    git rev-parse --verify --quiet "$base^{commit}" >/dev/null || {
        echo "coverage-pending: no '$base', skipping the shrink check" >&2
        exit 0
    }
    if ! git cat-file -e "$base:$LIST" 2>/dev/null; then
        echo "coverage-pending: '$LIST' is not on '$base' yet, nothing to compare" >&2
        exit 0
    fi
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    current_paths | sort -u >"$tmp/new"
    git show "$base:$LIST" | paths | sort -u >"$tmp/old"
    added=$(comm -23 "$tmp/new" "$tmp/old")
    if [ -n "$added" ]; then
        echo "$LIST may only shrink, but these paths are new relative to $base:" >&2
        echo "$added" >&2
        exit 1
    fi
    ;;
*)
    echo "usage: $0 regex | validate | shrink <base-ref>" >&2
    exit 2
    ;;
esac
