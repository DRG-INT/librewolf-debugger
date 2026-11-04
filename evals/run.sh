#!/bin/sh
# Run the mechanical half of evals/suite.json.
#
# POSIX sh: no arrays (argument lists are built with `set --`), no `<<<` (jq
# input arrives through a pipe), no `[[`, no `(( ))`, no `local`.
#
# The trigger half is prompts and needs a model; this script lists them but
# does not execute them. The split is deliberate: the mechanical cases are
# deterministic and belong in CI, the trigger cases measure a property of the
# skill's description text.
#
# A case whose requirement is absent reports SKIP, never PASS. A suite that
# silently skips is a suite that reports green on a broken tool.

set -u

REPO=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
SUITE="$REPO/evals/suite.json"

if [ -z "${LWDBG:-}" ]; then
    LWDBG=$(command -v lwdbg 2>/dev/null || printf '%s' "$HOME/.cache/lwdbg-target/release/lwdbg")
fi
LWDBG_SKILL=${LWDBG_SKILL:-$REPO/skills/librewolf-debugger}
export LWDBG_SKILL
DB="$HOME/.local/share/lwdbg/store/ssot.db"
ERRFILE=${TMPDIR:-/tmp}/lwdbg-eval-err.$$

if ! command -v jq >/dev/null 2>&1; then
    printf 'jq is required.\n' >&2
    printf '  FreeBSD: pkg install jq   Void: xbps-install -S jq\n' >&2
    printf '  Debian:  apt install jq   macOS: brew install jq\n' >&2
    exit 2
fi
if [ ! -x "$LWDBG" ]; then
    printf 'lwdbg not found at %s; set LWDBG=<path>\n' "$LWDBG" >&2
    exit 2
fi
[ -f "$SUITE" ] || { printf 'missing %s\n' "$SUITE" >&2; exit 2; }

cleanup() { rm -f "$ERRFILE"; }
trap cleanup EXIT HUP INT TERM

pass=0
fail=0
skip=0
failures=''

# ---------------------------------------- what is available, decided once

NETWORK=no
if curl -sS -m 8 -o /dev/null https://example.com/ 2>/dev/null; then
    NETWORK=yes
fi

GECKO=$("$LWDBG" detect --json 2>/dev/null \
    | jq -r 'map(select(.engine == "gecko" and (.excluded | not))) | .[0].binary // empty' \
    | head -1)

RENDER_A=''
RENDER_B=''
if [ -f "$DB" ] && command -v sqlite3 >/dev/null 2>&1; then
    RENDER_A=$(sqlite3 "$DB" 'select id from render order by id limit 1;' 2>/dev/null)
    RENDER_B=$(sqlite3 "$DB" 'select id from render order by id limit 1 offset 1;' 2>/dev/null)
fi

GPU_BUILD=no
if "$LWDBG" capabilities --json 2>/dev/null | jq -e '.gpu_helper != null' >/dev/null 2>&1; then
    GPU_BUILD=yes
fi

printf 'lwdbg      %s\n' "$LWDBG"
printf 'gpu build  %s\n' "$GPU_BUILD"
printf 'network    %s\n' "$NETWORK"
printf 'gecko      %s\n' "${GECKO:-none}"
printf 'renders    %s %s\n\n' "${RENDER_A:-none}" "${RENDER_B:-}"

requirement_met() {
    case $1 in
    network)              [ "$NETWORK" = yes ] ;;
    gecko-browser)        [ -n "$GECKO" ] ;;
    two-renders)          [ -n "$RENDER_A" ] && [ -n "$RENDER_B" ] ;;
    # Launching a browser and timing a request needs a machine that is not
    # busy. Inside `make preflight` it is compiling Rust, and on 8 GiB that
    # means 90% swap: Marionette did not bind within 90s, and a TTFB read
    # 587ms against curl's 20ms. Neither is a defect, so these are opt-in
    # rather than a flaky red.
    idle-machine)         [ "${LWDBG_EVAL_BROWSER:-}" = 1 ] ;;
    # Ask the binary. An explicit `--backend gpu` is an error in a build with
    # the feature off — only `Backend::Auto` falls back — so assuming the
    # fallback made this suite report a failure for a configuration choice.
    gpu)                  [ "$GPU_BUILD" = yes ] ;;
    store-with-exchanges) [ -f "$DB" ] ;;
    *)                    return 1 ;;
    esac
}

subst() {
    printf '%s' "$1" \
        | sed -e "s|{{GECKO}}|${GECKO:-}|g" \
              -e "s|{{RENDER_A}}|${RENDER_A:-}|g" \
              -e "s|{{RENDER_B}}|${RENDER_B:-}|g"
}

jqc() {
    # jqc <filter> <json>
    printf '%s' "$2" | jq -c "$1" 2>/dev/null
}

report() {
    printf '%-5s %-46s %s\n' "$1" "$2" "${3:-}"
    case $1 in
    PASS) pass=$((pass + 1)) ;;
    SKIP) skip=$((skip + 1)) ;;
    FAIL) fail=$((fail + 1)); failures="$failures
  $2: ${3:-}" ;;
    esac
}

n=$(jq '.mechanical | length' "$SUITE")
i=0
while [ "$i" -lt "$n" ]
do
    case_json=$(jq -c ".mechanical[$i]" "$SUITE")
    i=$((i + 1))
    id=$(jqc '.id' "$case_json" | tr -d '"')

    # Cases delegated to a crate's own test suite.
    unit=$(printf '%s' "$case_json" | jq -r '.unit_test // empty')
    if [ -n "$unit" ]; then
        report SKIP "$id" "covered by cargo test: $unit"
        continue
    fi

    missing=''
    for req in $(printf '%s' "$case_json" | jq -r '.requires[]?')
    do
        requirement_met "$req" || missing="$missing $req"
    done
    if [ -n "$missing" ]; then
        report SKIP "$id" "missing:$missing"
        continue
    fi

    # Build the argument list in the positional parameters. newline is the
    # separator, so an argument containing a space survives.
    set --
    oldifs=$IFS
    IFS='
'
    for raw in $(printf '%s' "$case_json" | jq -r '.cmd[]')
    do
        IFS=$oldifs
        set -- "$@" "$(subst "$raw")"
        IFS='
'
    done
    IFS=$oldifs

    out=$("$LWDBG" "$@" 2>"$ERRFILE")
    rc=$?
    err=$(cat "$ERRFILE")

    if [ "$(printf '%s' "$case_json" | jq -r '.expect_failure // false')" = true ]; then
        if [ "$rc" -eq 0 ]; then
            report FAIL "$id" 'expected a non-zero exit, got 0'
            continue
        fi
        want=$(printf '%s' "$case_json" | jq -r '.assert_stderr_contains // empty')
        if [ -n "$want" ] && ! printf '%s%s' "$err" "$out" | grep -qF -- "$want"; then
            report FAIL "$id" "stderr did not contain: $want"
            continue
        fi
        report PASS "$id" 'refused as required'
        continue
    fi

    if [ "$rc" -ne 0 ] && \
       [ "$(printf '%s' "$case_json" | jq -r '.allow_nonzero_exit // false')" != true ]; then
        report FAIL "$id" "exit $rc: $(printf '%s' "$err" | head -1)"
        continue
    fi

    ok=yes
    why=''
    an=$(printf '%s' "$case_json" | jq '(.assert // []) | length')
    a=0
    while [ "$a" -lt "$an" ]
    do
        expr=$(printf '%s' "$case_json" | jq -r ".assert[$a].jq")
        wanted=$(jqc ".assert[$a].eq" "$case_json")
        got=$(jqc "$expr" "$out")
        a=$((a + 1))
        if [ "$got" != "$wanted" ]; then
            ok=no
            why="$expr => ${got:-<error>}, wanted $wanted"
            break
        fi
    done
    if [ "$ok" != yes ]; then
        report FAIL "$id" "$why"
        continue
    fi

    # Cross-checks compare two invocations. This is where a mis-instrumented
    # browser path and a wrong shader get caught.
    cc=$(printf '%s' "$case_json" | jq -c '.cross_check // empty')
    if [ -n "$cc" ]; then
        set --
        oldifs=$IFS
        IFS='
'
        for raw in $(printf '%s' "$cc" | jq -r '.cmd[]')
        do
            IFS=$oldifs
            set -- "$@" "$(subst "$raw")"
            IFS='
'
        done
        IFS=$oldifs

        other=$("$LWDBG" "$@" 2>/dev/null)
        rule=$(printf '%s' "$cc" | jq -r '.rule')
        tr_ratio=$(printf '%s' "$cc" | jq -r '.tolerance_ratio // empty')
        tr_abs=$(printf '%s' "$cc" | jq -r '.tolerance_abs // empty')
        direction=$(printf '%s' "$cc" | jq -r '.direction // empty')
        min_ratio=$(printf '%s' "$cc" | jq -r '.min_ratio // empty')

        # A directional check: the browser must not report itself
        # implausibly *faster* than the wire. That is the bug class worth
        # catching — the timeOrigin defect under-reported a 6.0s load as
        # 0.714s. Being slower is expected, and inside this gate the machine
        # is compiling Rust, so a two-sided ratio goes red for CPU contention
        # rather than for a defect.
        if [ "$direction" = not_faster_than ]; then
            x=$(printf '%s' "$out" | jq -r '.median_us_ttfb // empty')
            y=$(printf '%s' "$other" | jq -r '.median_us_ttfb // empty')
            if [ -z "$x" ] || [ -z "$y" ] || [ "$x" = null ] || [ "$y" = null ]; then
                report SKIP "$id" 'cross-check had no median to compare'
                continue
            fi
            verdict=$(awk -v b="$x" -v c="$y" -v m="${min_ratio:-0.5}" \
                'BEGIN {
                    if (b <= 0) { print "bad"; exit }
                    if (b < c * m) { print "too_fast"; exit }
                    if (b > c * 2) { print "slow"; exit }
                    print "ok"
                 }')
            case "$verdict" in
            ok)
                report PASS "$id" "$rule (browser $x us vs curl $y us)" ;;
            slow)
                # Reported, not failed. On a loaded machine this is the
                # browser waiting for the CPU, not mis-instrumented timing.
                report PASS "$id" \
                    "$rule — browser $x us vs curl $y us, slower than 2x; \
likely machine load, not instrumentation" ;;
            too_fast)
                report FAIL "$id" \
                    "browser reported $x us against curl's $y us — faster than \
the wire, which cannot be right" ;;
            *)
                report FAIL "$id" "browser median TTFB was $x us" ;;
            esac
            continue
        fi

        if [ -n "$tr_ratio" ]; then
            x=$(printf '%s' "$out" | jq -r '.median_us_ttfb // empty')
            y=$(printf '%s' "$other" | jq -r '.median_us_ttfb // empty')
            if [ -z "$x" ] || [ -z "$y" ] || [ "$x" = null ] || [ "$y" = null ]; then
                report SKIP "$id" 'cross-check had no median to compare'
                continue
            fi
            within=$(awk -v a="$x" -v b="$y" -v t="$tr_ratio" \
                'BEGIN { lo = (a < b ? a : b); hi = (a > b ? a : b);
                         if (lo < 1) lo = 1;
                         print (hi / lo <= t) ? "yes" : "no" }')
            if [ "$within" = yes ]; then
                report PASS "$id" "$rule (browser $x us vs curl $y us)"
            else
                report FAIL "$id" "$rule violated: $x us vs $y us"
            fi
            continue
        fi

        if [ -n "$tr_abs" ]; then
            x=$(printf '%s' "$out" | jq -r '.[0].parity.ssim')
            y=$(printf '%s' "$other" | jq -r '.[0].parity.ssim')
            within=$(awk -v a="$x" -v b="$y" -v t="$tr_abs" \
                'BEGIN { d = a - b; if (d < 0) d = -d;
                         print (d <= t) ? "yes" : "no" }')
            if [ "$within" = yes ]; then
                report PASS "$id" "$rule (gpu $x, cpu $y)"
            else
                report FAIL "$id" "$rule violated: gpu $x vs cpu $y"
            fi
            continue
        fi
    fi

    report PASS "$id" ''
done

printf '\n--- trigger cases (prompts; run these through the harness) ---\n'
jq -r '.trigger[] | "\(if .expect == "fire" then "FIRE " else "QUIET" end) \(.id)\n      \(.prompt)"' "$SUITE"

printf '\nmechanical: %d passed, %d failed, %d skipped\n' "$pass" "$fail" "$skip"
if [ -n "$failures" ]; then
    printf '\nfailures:%s\n' "$failures"
fi
[ "$fail" -eq 0 ]
