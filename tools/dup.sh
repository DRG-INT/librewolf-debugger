#!/bin/sh
# Find duplicated blocks of code.
#
# `jscpd` is not installed and pulling a node tree in for one report is a poor
# trade, so this is a windowed exact-match detector in awk: normalise each
# line, slide an N-line window over the file, and report every window whose
# text occurs more than once.
#
# It is cruder than a token-level tool — it will not spot a duplicate that was
# reindented and had its variables renamed — and it is enough for the thing
# that actually happens: a block written twice by copy-paste.
#
#   tools/dup.sh [window-size]        default 8
#
# Exit status is 0 whether or not duplicates are found. This is a report, not
# a gate: some duplication is correct (three transport drivers *should* look
# alike at the edges) and the judgement is a human's.

set -eu

REPO=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
WIN=${1:-8}

cd "$REPO"

# Normalisation, in order: drop leading indentation, collapse runs of
# whitespace, drop line comments, drop string contents. Dropping string
# contents is what lets this see two blocks that differ only in their error
# messages — which is exactly the shape of copied code.
#
# Lines that normalise to nothing, or to a single brace, are skipped: a run of
# eight closing braces is not a duplicated block.
find . -type f \( -name '*.rs' -o -name '*.sh' \) \
    ! -path './target/*' ! -path './dist/*' ! -path './.git/*' \
    ! -path './audit/*' -print |
sort |
while IFS= read -r f
do
    awk -v file="$f" -v win="$WIN" '
    {
        raw = $0
        line = raw
        sub(/^[ \t]+/, "", line)
        gsub(/[ \t]+/, " ", line)
        sub(/\/\/.*$/, "", line)        # rust line comment
        sub(/#.*$/, "", line)           # shell comment
        gsub(/"[^"]*"/, "\"S\"", line)  # string contents
        gsub(/[ \t]+$/, "", line)
        if (line == "" || line == "{" || line == "}" || line == "};" ||
            line == ")" || line == ");" || line == "] ") next
        n++
        text[n] = line
        lineno[n] = NR
    }
    END {
        for (i = 1; i + win - 1 <= n; i++) {
            key = ""
            fields = 0
            for (j = 0; j < win; j++) {
                t = text[i + j]
                key = key t "\x01"
                # `name: value,` — the signature of one entry in a table of
                # struct literals. Repeating field names across entries is
                # what a table *is*, not a copy-paste defect.
                if (t ~ /^[A-Za-z_][A-Za-z_0-9]*: .*,$/) fields++
            }
            if (fields * 2 > win) continue
            printf "%s\t%s:%d\n", key, file, lineno[i]
        }
    }' "$f"
done |
sort |
awk -F'\t' '
{
    if ($1 == prev) {
        count++
        locs = locs ", " $2
    } else {
        if (count > 1) print count "\t" first "\t" locs
        prev = $1; first = $2; locs = $2; count = 1
    }
}
END { if (count > 1) print count "\t" first "\t" locs }
' |
sort -rn |
awk -F'\t' -v win="$WIN" '
BEGIN {
    printf "Duplicated %d-line blocks, most-repeated first:\n\n", win
    found = 0
}
{
    found++
    # Only the first occurrence of each overlapping family is interesting;
    # a 12-line duplicate shows up as five overlapping 8-line windows.
    split($3, parts, ", ")
    key = parts[1]
    sub(/:[0-9]+$/, "", key)
    second = parts[2]
    sub(/:[0-9]+$/, "", second)
    group = key "|" second
    if (group in seen) next
    seen[group] = 1
    printf "  x%-3s %s\n", $1, $3
}
END {
    if (found == 0) print "  none"
    printf "\nNote: three transport drivers legitimately share shape at the\n"
    printf "edges. Judge each hit; this is a report, not a gate.\n"
}'
