#!/bin/sh
# Merge several SARIF files into one.
#
# SARIF 2.1.0 holds an array of `runs`, each with its own `tool.driver`, so a
# merge is a concatenation of those arrays — no rewriting of results, and every
# finding keeps the identity of the tool that produced it.
#
#   tools/sarif-merge.sh audit/report.sarif audit/clippy.sarif audit/...
#
# A missing or empty input is skipped with a note on stderr rather than
# failing: not every tool applies to every repository, and a merge that
# refused to run because `bandit` had nothing to say would be useless here.

set -eu

if [ $# -lt 2 ]; then
    printf 'usage: %s <output.sarif> <input.sarif>...\n' "$(basename -- "$0")" >&2
    exit 2
fi

out=$1
shift

command -v jq >/dev/null 2>&1 || { printf 'jq is required\n' >&2; exit 2; }

tmp="${out}.part$$"
trap 'rm -f "$tmp"' EXIT HUP INT TERM

: > "$tmp"
kept=0
for f
do
    if [ ! -s "$f" ]; then
        printf '  skip %s (missing or empty)\n' "$f" >&2
        continue
    fi
    if ! jq -e '.runs | type == "array"' "$f" >/dev/null 2>&1; then
        printf '  skip %s (no runs array — not SARIF?)\n' "$f" >&2
        continue
    fi
    n=$(jq '[.runs[].results[]? | select((.suppressions // []) | length == 0)] | length' "$f")
    sup=$(jq '[.runs[].results[]? | select((.suppressions // []) | length > 0)] | length' "$f")
    printf '  take %-28s %s open, %s suppressed\n' "$f" "$n" "$sup" >&2
    printf '%s\n' "$f" >> "$tmp"
    kept=$((kept + 1))
done

if [ "$kept" -eq 0 ]; then
    printf 'nothing to merge\n' >&2
    exit 1
fi

# `jq -s` slurps the inputs into an array; the runs are then flattened. The
# $schema and version come from the first input so the result declares the
# same dialect its contents were written in.
# shellcheck disable=SC2046  # the file list is newline-separated by construction
jq -s '{
    "$schema": (.[0]["$schema"] // "https://json.schemastore.org/sarif-2.1.0.json"),
    version: (.[0].version // "2.1.0"),
    runs: (map(.runs) | add)
}' $(tr '\n' ' ' < "$tmp") > "$out"

open=$(jq '[.runs[].results[]? | select((.suppressions // []) | length == 0)] | length' "$out")
sup=$(jq '[.runs[].results[]? | select((.suppressions // []) | length > 0)] | length' "$out")
runs=$(jq '.runs | length' "$out")
printf 'merged %s run(s): %s open, %s suppressed -> %s\n' "$runs" "$open" "$sup" "$out" >&2

# The exit status is the gate: open findings fail, suppressed ones do not.
# A suppression carries its reason at the site, so it has already been judged.
[ "$open" -eq 0 ]
