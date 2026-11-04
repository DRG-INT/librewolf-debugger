#!/bin/sh
# Rule-driven audit of this repository's own artifacts, with auto-remediation.
#
# The shape is deliberate: a rule table per artifact kind, each rule saying
# what it checks and why it matters, a pass that applies the safe fixes, and a
# re-run that must come back clean. The rules that cannot be fixed mechanically
# are reported for a person — nothing here silently rewrites a shader's maths
# or a measured number in a document.
#
#   tools/audit_rules.sh            report only
#   tools/audit_rules.sh --fix      apply the safe fixes, then re-audit
#
# Exit status: 0 when no rule is violated, 1 otherwise. That makes it usable
# as a gate, unlike tools/dup.sh which is a report.

set -eu

REPO=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$REPO"

FIX=no
[ "${1:-}" = "--fix" ] && FIX=yes

fixed=0
checked=0

# Failures are recorded in a file rather than a variable: several of the
# checks below read from a pipeline, which puts them in a subshell where an
# incremented counter would be discarded at the end of the loop.
FAILFILE=${TMPDIR:-/tmp}/lwdbg-audit-fails.$$
: > "$FAILFILE"
trap 'rm -f "$FAILFILE"' EXIT HUP INT TERM

# fail <rule-id> <file> <why>
fail() {
    printf '  FAIL %-28s %s\n' "$1" "$2"
    printf '       %s\n' "$3"
    printf '%s\t%s\n' "$1" "$2" >> "$FAILFILE"
}
pass_() { checked=$((checked + 1)); }
note() { printf '  note %-28s %s\n' "$1" "$2"; }
did_fix() {
    printf '  FIX  %-28s %s\n' "$1" "$2"
    fixed=$((fixed + 1))
}

printf 'Artifact rule audit (fix=%s)\n\n' "$FIX"

# =========================================================== WGSL shaders
# The shader is compared against a CPU reference in the test suite, so its
# maths is covered. What the tests cannot see is a binding declared in one
# place and not the other, or a dispatch that disagrees with the workgroup
# size — both produce wrong numbers rather than an error.
printf '%s\n' '-- wgsl'
for f in gpu/shaders/*.wgsl
do
    [ -f "$f" ] || continue
    pass_

    # wgsl-bindings-declared: every @binding in the shader must appear in the
    # host-side bind group layout. A missing entry is a validation error at
    # pipeline creation, which surfaces far from the cause.
    grep -oE '@binding\([0-9]+\)' "$f" | grep -oE '[0-9]+' | sort -u |
    while IFS= read -r b
    do
        if ! grep -q "storage_entry($b," gpu/src/gpu.rs 2>/dev/null &&
           ! grep -q "binding: $b," gpu/src/gpu.rs 2>/dev/null; then
            fail wgsl-bindings-declared "$f" \
                "@binding($b) has no matching entry in gpu/src/gpu.rs layout"
        fi
    done

    # wgsl-workgroup-matches-dispatch: the host rounds the grid up to the
    # workgroup size. If the two disagree, some windows are never written and
    # the mean is computed over uninitialised memory.
    wg=$(grep -oE '@workgroup_size\([0-9]+' "$f" | grep -oE '[0-9]+' | head -1 || true)
    if [ -n "${wg:-}" ]; then
        if ! grep -qE "(\+ $((wg - 1))\) / $wg|div_ceil\($wg\))" gpu/src/gpu.rs 2>/dev/null; then
            fail wgsl-workgroup-matches-dispatch "$f" \
                "@workgroup_size($wg) but gpu.rs does not round the dispatch up by $wg"
        fi
    fi

    # wgsl-bounds-guard: a compute entry point must return early for
    # invocations outside the data, because the grid is rounded up.
    if grep -q '@compute' "$f" && ! grep -qE '^\s*(if|return)' "$f"; then
        fail wgsl-bounds-guard "$f" "compute entry point with no early return"
    fi

    # wgsl-no-texture-sample: sampling needs a sampler binding this pipeline
    # does not create.
    grep -q 'textureSample' "$f" &&
        fail wgsl-no-texture-sample "$f" "textureSample in a compute shader"
done

# ======================================================= WebExtension JS
# This runs inside the user's real browser session, so the rules are about
# what it is allowed to reach rather than about style.
printf '%s\n' '-- extension'
for f in extension/*.js
do
    [ -f "$f" ] || continue
    pass_

    # ext-no-dynamic-code: the whole point of a fixed dispatch table is that
    # no message can become code.
    for bad in 'eval(' 'new Function' 'innerHTML' 'document.write' 'setTimeout("'
    do
        grep -F "$bad" "$f" >/dev/null 2>&1 &&
            fail ext-no-dynamic-code "$f" "contains $bad"
    done

    # ext-no-network: its only channel is the native messaging port. Network
    # access would be a far larger grant than the job needs.
    for bad in 'fetch(' 'XMLHttpRequest' 'new WebSocket' 'navigator.sendBeacon'
    do
        grep -F "$bad" "$f" >/dev/null 2>&1 &&
            fail ext-no-network "$f" "contains $bad"
    done

    # ext-commands-declare-mutates: the host's write gate reads this flag. A
    # command without it is treated as mutating, which is safe, but silently —
    # so the omission is reported.
    ncmd=$(grep -cE '^  [a-zA-Z]+: \{$' "$f" || true)
    nmut=$(grep -cE '^    mutates: (true|false),$' "$f" || true)
    if [ "$ncmd" -gt 0 ] && [ "$ncmd" -ne "$nmut" ]; then
        fail ext-commands-declare-mutates "$f" \
            "$ncmd command(s) but $nmut mutates: declaration(s)"
    fi

    # ext-browser-namespace: chrome.* is the Blink spelling; on Gecko it
    # exists but without promises, so awaits silently never resolve.
    grep -qE '(^|[^a-zA-Z.])chrome\.' "$f" &&
        fail ext-browser-namespace "$f" "uses chrome.* instead of browser.*"
done

# ============================================================= reference docs
# These are the context another agent reads instead of guessing. A dead
# pointer in here is worse than a missing document: it looks like coverage.
printf '%s\n' '-- skills'
SKILL=skills/librewolf-debugger
if [ -f "$SKILL/SKILL.md" ]; then
    pass_

    # skills-references-exist: every reference named in SKILL.md is there.
    grep -oE 'references/[a-z-]+\.md' "$SKILL/SKILL.md" | sort -u |
    while IFS= read -r r
    do
        [ -f "$SKILL/$r" ] ||
            fail skills-references-exist "$r" "named in SKILL.md but absent"
    done

    # skills-no-orphans: a reference nothing points to will never be read.
    for r in "$SKILL"/references/*.md
    do
        [ -f "$r" ] || continue
        b=$(basename -- "$r")
        if ! grep -q "references/$b" "$SKILL/SKILL.md" &&
           ! grep -rq "references/$b" "$SKILL"/references/ 2>/dev/null; then
            fail skills-no-orphans "$b" "no document points to it; it will never be loaded"
        fi
    done

    # skills-no-machine-paths: an absolute path that exists only here is a
    # dead pointer on anybody else's machine. /Applications and /usr are
    # genuinely portable; /Volumes and /Users are not.
    for r in "$SKILL/SKILL.md" "$SKILL"/references/*.md
    do
        [ -f "$r" ] || continue
        if grep -qE '(^|[^a-zA-Z/])/(Volumes|Users)/' "$r"; then
            n=$(grep -cE '(^|[^a-zA-Z/])/(Volumes|Users)/' "$r")
            note skills-no-machine-paths "$(basename -- "$r"): $n machine-specific path(s)"
        fi
    done

    # skills-budget: SKILL.md is loaded on every turn that triggers the skill,
    # so its length is a running cost. References are loaded on demand and
    # may be long.
    n=$(wc -l < "$SKILL/SKILL.md" | tr -d ' ')
    if [ "$n" -gt 120 ]; then
        fail skills-budget "SKILL.md" "$n lines, over the 120-line always-loaded budget"
    else
        note skills-budget "SKILL.md $n/120 lines"
    fi
fi

# ================================================================ shell
printf '%s\n' '-- shell'
for f in configure scripts/*.sh evals/run.sh tools/*.sh
do
    [ -f "$f" ] || continue
    pass_

    # sh-posix-only: these run on FreeBSD's /bin/sh and in a Debian container
    # where sh is dash. A bashism there fails at run time, not at review.
    #
    # Three exclusions, each for a false positive this rule produced on its
    # first run:
    #   * comment lines, including the ones that *name* the bashisms;
    #   * `[[:space:]]` and friends — POSIX character classes, not bash tests;
    #   * this script, which necessarily contains every pattern it searches for.
    case "$f" in tools/audit_rules.sh) continue ;; esac
    for pat in '\[\[[^:]' 'declare -' 'typeset ' '<<<' 'mapfile' '\$\{[A-Za-z_]+\[@\]\}'
    do
        if grep -nE "$pat" "$f" 2>/dev/null |
           grep -vE '^[0-9]+:[[:space:]]*#' >/dev/null; then
            fail sh-posix-only "$f" "contains a bashism matching $pat"
        fi
    done

    # sh-set-eu: without these, a failing command in the middle of a script
    # is invisible and the rest runs on bad state.
    if ! grep -qE '^set -eu|^set -e$|^set -u$' "$f"; then
        if [ "$FIX" = yes ]; then
            awk 'NR==1{print; print ""; print "set -eu"; next} {print}' "$f" > "$f.tmp$$" &&
                mv -f "$f.tmp$$" "$f" && did_fix sh-set-eu "$f"
        else
            fail sh-set-eu "$f" "no 'set -eu'"
        fi
    fi

    # sh-shebang: a script run as ./x needs one, and it must name sh.
    head -1 "$f" | grep -qE '^#!/(bin/sh|usr/bin/env sh)$' ||
        fail sh-shebang "$f" "shebang is not #!/bin/sh"
done

# ============================================================== Makefile.in
printf '%s\n' '-- make'
if [ -f Makefile.in ]; then
    pass_
    # make-posix-only: bmake on FreeBSD does not understand GNU extensions,
    # and the whole point of this build system is that it runs there.
    # The `?` in `?=` must be escaped: in ERE `[^#]*?=` parses as
    # `([^#]*)?=`, which matches any line containing an `=` at all. That
    # false positive is why this reads the way it does.
    for pat in ':=' '\?=' '\+=' 'ifeq' 'ifdef' '\$\(shell'
    do
        if grep -vE '^[[:space:]]*#' Makefile.in |
           grep -qE "$pat" 2>/dev/null; then
            fail make-posix-only Makefile.in "contains the GNU-only construct $pat"
        fi
    done
    # make-no-install-prog: POSIX does not specify install(1), and GNU's -D
    # does not exist on FreeBSD.
    grep -nE '^\s+install -' Makefile.in >/dev/null 2>&1 &&
        fail make-no-install-prog Makefile.in "uses install(1)"
    # make-destdir: a staged install is how every packager consumes this.
    grep -q 'DESTDIR' Makefile.in ||
        fail make-destdir Makefile.in "no DESTDIR support"
fi

# ==================================================================== rust
printf '%s\n' '-- rust'
# rust-unsafe-documented: Rust convention, and the only unsafe block here is
# an FFI call whose obligations have to be written down to be checkable.
find . -name '*.rs' ! -path './target/*' ! -path './.git/*' -print |
while IFS= read -r f
do
    grep -n '^[[:space:]]*unsafe[[:space:]]*{' "$f" 2>/dev/null |
    while IFS=: read -r ln _
    do
        start=$((ln - 24))
        [ "$start" -lt 1 ] && start=1
        if ! sed -n "${start},${ln}p" "$f" | grep -q 'SAFETY:'; then
            fail rust-unsafe-documented "$f:$ln" \
                "unsafe block with no SAFETY comment within 24 lines above it"
        fi
    done
done
pass_

# rust-errors-say-what-next: a bail! that only states what went wrong leaves
# the reader stuck. This is a report, not a gate — judgement, not a pattern.
nbail=$(grep -rhoE 'bail!\("[^"]{0,40}"' --include='*.rs' . 2>/dev/null | wc -l | tr -d ' ')
[ "$nbail" -gt 0 ] && note rust-errors-say-what-next "$nbail short bail! message(s) worth re-reading"

fails=$(wc -l < "$FAILFILE" | tr -d ' ')
printf '\n%s rule group(s) checked, %s violation(s), %s fix(es) applied\n' \
    "$checked" "$fails" "$fixed"

if [ "$FIX" = yes ] && [ "$fixed" -gt 0 ]; then
    printf '\nre-auditing after fixes:\n'
    exec "$0"
fi

[ "$fails" -eq 0 ]
