#!/bin/sh
# Generate audit/catalog.json — the analysis tools this repository could use,
# whether each is installed, and whether it has anything to scan here.
#
# The three-way distinction is the whole point. A report that shows a green
# tick for a Python linter in a repository with no Python is worse than no
# report: it looks like coverage and is not. So every entry carries:
#
#   installed : is the binary on this machine
#   applies   : does this repository contain files the tool reads
#   status     : ok | not-installed | nothing-to-scan
#
# The tool list is seeded from the analysis-tools.dev categories (linter,
# formatter, SAST, secrets, licence, duplication) narrowed to the languages
# that actually occur here, plus the Rust-specific tools no general catalogue
# lists.

set -eu

REPO=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
OUT=${1:-$REPO/audit/catalog.json}
mkdir -p "$(dirname -- "$OUT")"

# --- what languages are actually present -----------------------------------
#
# Counted once, excluding build output, so `applies` is a fact about this
# repository rather than a guess from its name.
count_ext() {
    find "$REPO" -type f -name "*.$1" \
        ! -path "$REPO/target/*" ! -path "$REPO/dist/*" \
        ! -path "$REPO/.git/*" ! -path "$REPO/audit/*" 2>/dev/null | wc -l | tr -d ' '
}

N_RS=$(count_ext rs)
N_WGSL=$(count_ext wgsl)
N_JS=$(count_ext js)
N_MD=$(count_ext md)
N_SH=$(count_ext sh)
N_TOML=$(count_ext toml)
N_JSON=$(count_ext json)
N_SQL=$(count_ext sql)
N_PY=$(count_ext py)
N_TS=$(count_ext ts)
N_YML=$(count_ext yml)
N_MAN=$(find "$REPO/doc" -type f -name "*.1" 2>/dev/null | wc -l | tr -d ' ')
N_DOCKER=$(find "$REPO" -name Dockerfile ! -path "$REPO/target/*" 2>/dev/null | wc -l | tr -d ' ')
N_WF=$(find "$REPO/.github/workflows" -type f 2>/dev/null | wc -l | tr -d ' ')

where() { command -v "$1" 2>/dev/null || true; }

# emit <name> <kind> <languages> <bin> <applies-count> <sarif> <covers>
emit() {
    e_name=$1; e_kind=$2; e_langs=$3; e_bin=$4; e_n=$5; e_sarif=$6; e_covers=$7
    e_path=$(where "$e_bin")
    if [ -n "$e_path" ]; then e_inst=true; else e_inst=false; fi
    if [ "$e_n" -gt 0 ] 2>/dev/null; then e_app=true; else e_app=false; fi
    if [ "$e_inst" = true ] && [ "$e_app" = true ]; then
        e_status=ok
    elif [ "$e_inst" = false ]; then
        e_status=not-installed
    else
        e_status=nothing-to-scan
    fi
    printf '%s' "$sep"
    printf '{"name":"%s","kind":"%s","languages":[%s],"binary":"%s",' \
        "$e_name" "$e_kind" "$e_langs" "$e_bin"
    printf '"installed":%s,"path":"%s","applies":%s,"files":%s,' \
        "$e_inst" "$e_path" "$e_app" "$e_n"
    printf '"sarif":"%s","status":"%s","covers":[%s]}' \
        "$e_sarif" "$e_status" "$e_covers"
    sep=','
}

sep=''
{
printf '['
# ---- Rust ------------------------------------------------------------------
emit clippy          linter    '"rust"' cargo-clippy   "$N_RS"   'via clippy-sarif' '"correctness","suspicious","perf","complexity","style"'
emit rustfmt         formatter '"rust"' rustfmt        "$N_RS"   none            '"formatting"'
emit cargo-audit     advisory  '"rust"' cargo-audit    "$N_RS"   none            '"RustSec advisories","yanked crates"'
emit cargo-deny      policy    '"rust"' cargo-deny     "$N_RS"   none            '"advisories","licences","bans","sources"'
emit cargo-geiger    sast      '"rust"' cargo-geiger   "$N_RS"   none            '"unsafe usage"'
emit cargo-machete   hygiene   '"rust"' cargo-machete  "$N_RS"   none            '"unused dependencies"'
emit cargo-outdated  hygiene   '"rust"' cargo-outdated "$N_RS"   none            '"stale dependencies"'
emit cargo-llvm-cov  coverage  '"rust"' cargo-llvm-cov "$N_RS"   none            '"line and region coverage"'
emit cargo-miri      sast      '"rust"' cargo-miri     "$N_RS"   none            '"UB in unsafe code"'
emit cargo-udeps     hygiene   '"rust"' cargo-udeps    "$N_RS"   none            '"unused dependencies, nightly"'
# ---- shell -----------------------------------------------------------------
emit shellcheck      linter    '"sh"'   shellcheck     "$N_SH"   'via shellcheck-sarif' '"portability","quoting","correctness"'
emit checkbashisms   linter    '"sh"'   checkbashisms  "$N_SH"   none            '"bashisms in POSIX sh"'
# ---- markdown and prose ----------------------------------------------------
emit markdownlint    linter    '"markdown"' markdownlint "$N_MD" none            '"structure","consistency"'
emit codespell       linter    '"any"'  codespell      "$N_MD"   none            '"misspellings"'
emit typos           linter    '"any"'  typos          "$N_MD"   none            '"misspellings, faster"'
emit vale            prose     '"markdown"' vale       "$N_MD"   none            '"prose style"'
emit mandoc          linter    '"mdoc"' mandoc         "$N_MAN"  none            '"man page syntax and order"'
# ---- secrets ---------------------------------------------------------------
emit gitleaks        secrets   '"any"'  gitleaks       "$N_RS"   native          '"credentials","keys","tokens"'
emit trufflehog      secrets   '"any"'  trufflehog     "$N_RS"   none            '"credentials, with verification"'
# ---- multi-language SAST ---------------------------------------------------
emit semgrep         sast      '"rust","js","any"' semgrep "$N_RS" native '"insecure patterns","secrets"'
# ---- duplication -----------------------------------------------------------
emit jscpd           duplication '"any"' jscpd         "$N_RS"   none            '"copy-paste blocks"'
emit tokei           metrics   '"any"'  tokei          "$N_RS"   none            '"lines of code by language"'
# ---- javascript ------------------------------------------------------------
emit eslint          linter    '"js"'   eslint         "$N_JS"   none            '"correctness","unused code"'
emit biome           linter    '"js"'   biome          "$N_JS"   none            '"lint and format"'
# ---- things this repository does not contain -------------------------------
emit bandit          sast      '"python"' bandit       "$N_PY"   none            '"insecure Python patterns"'
emit ruff            linter    '"python"' ruff         "$N_PY"   none            '"Python lint"'
emit hadolint        linter    '"dockerfile"' hadolint "$N_DOCKER" none          '"Dockerfile best practice"'
emit actionlint      linter    '"yaml"' actionlint     "$N_WF"   none            '"GitHub Actions workflows"'
emit tsc             typecheck '"ts"'   tsc            "$N_TS"   none            '"TypeScript types"'
emit yamllint        linter    '"yaml"' yamllint       "$N_YML"  none            '"YAML syntax and style"'
emit sqlfluff        linter    '"sql"'  sqlfluff       "$N_SQL"  none            '"SQL dialect and style"'
emit taplo          formatter '"toml"'  taplo          "$N_TOML" none            '"TOML format and schema"'
# ---- SARIF plumbing --------------------------------------------------------
emit clippy-sarif    converter '"rust"' clippy-sarif   "$N_RS"   produces        '"clippy json -> SARIF"'
emit shellcheck-sarif converter '"sh"'  shellcheck-sarif "$N_SH" produces        '"shellcheck json -> SARIF"'
emit sarif-fmt       reporter  '"any"'  sarif-fmt      "$N_RS"   consumes        '"human-readable SARIF"'
printf ']\n'
} > "$OUT.part$$"

if command -v jq >/dev/null 2>&1; then
    jq . "$OUT.part$$" > "$OUT" && rm -f "$OUT.part$$"
else
    mv -f "$OUT.part$$" "$OUT"
fi

printf 'wrote %s\n' "$OUT"
printf '  languages here: rs=%s wgsl=%s js=%s md=%s sh=%s toml=%s json=%s sql=%s mdoc=%s\n' \
    "$N_RS" "$N_WGSL" "$N_JS" "$N_MD" "$N_SH" "$N_TOML" "$N_JSON" "$N_SQL" "$N_MAN"
printf '  absent here:    py=%s ts=%s yml=%s Dockerfile=%s workflows=%s\n' \
    "$N_PY" "$N_TS" "$N_YML" "$N_DOCKER" "$N_WF"

if command -v jq >/dev/null 2>&1; then
    printf '\n  %-16s %s\n' status count
    jq -r 'group_by(.status) | map("  \(.[0].status)\t\(length)") | .[]' "$OUT" |
        while IFS="$(printf '\t')" read -r s c; do printf '  %-16s %s\n' "$s" "$c"; done
    printf '\n  usable right now:\n'
    jq -r '.[] | select(.status == "ok") | "    \(.name)"' "$OUT"
fi
