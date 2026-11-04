#!/bin/sh
# Export every tool call — past, present and future — into one console log.
#
#   tools/console-log.sh backfill      # every past call, from the transcripts
#   tools/console-log.sh hook          # one call; invoked by a Claude Code hook
#   tools/console-log.sh install-hook  # wire the hook so future calls land too
#   tools/console-log.sh matrix        # summary matrices over the log
#   tools/console-log.sh drives        # volume and device detail
#
# Default output: audit/console.log, overridable with --out or $CONSOLE_LOG.
#
# Where the past comes from. Claude Code writes a JSONL transcript per session
# under ~/.claude/projects/<slug>/<session-uuid>.jsonl. Each tool invocation is
# a `tool_use` content block on an `assistant` record, and its outcome is a
# `tool_result` block on the following `user` record, joined by `tool_use_id`.
# That join is what makes a duration, an exit state and an output size
# available at all — neither record has them alone.
#
# Fields per row, and why each is there rather than nice-to-have:
#
#   ts          start, from the tool_use record
#   dur_ms      result timestamp minus start; the only timing that exists
#   tool        Bash, Read, Edit, …
#   verdict     ok | ERR | INTR — is_error and interrupted are separate facts
#   perm        the permission decision; a denied call looks like a failure
#               otherwise, and it is not one
#   sbx         whether it ran sandboxed, which changes what a failure means
#   out/err     byte counts, not content: the content is the transcript's job
#   vol         the volume the cwd sat on. This project spans three with very
#               different latency, and that has already explained two
#               "hangs" (a 13-minute single-crate build, a stalled link)
#   cwd, cmd    what was actually run
#
# POSIX sh and jq. jq rather than awk because the input is 7 MB of nested
# JSON and an awk parser for it would be the least reliable thing here.

set -eu

REPO=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
MODE=${1:-backfill}
[ $# -gt 0 ] && shift

OUT=${CONSOLE_LOG:-$REPO/audit/console.log}
PROJECTS="$HOME/.claude/projects"
ALL_PROJECTS=no
ONLY_PROJECT=

while [ $# -gt 0 ]
do
    case $1 in
    --out) OUT=$2; shift 2 ;;
    --out=*) OUT=${1#*=}; shift ;;
    --all-projects) ALL_PROJECTS=yes; shift ;;
    --project) ONLY_PROJECT=$2; shift 2 ;;
    --project=*) ONLY_PROJECT=${1#*=}; shift ;;
    *) printf 'unknown option: %s\n' "$1" >&2; exit 2 ;;
    esac
done

mkdir -p -- "$(dirname -- "$OUT")"

command -v jq >/dev/null 2>&1 || { printf 'jq is required\n' >&2; exit 2; }

# --------------------------------------------------------------- dividers
#
# Box drawing rather than dashes: the rows carry commands that contain dashes,
# pipes and equals signs, and a divider that can be confused with content is
# not a divider.

RULE_HEAVY='━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━'
RULE_LIGHT='────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────'
RULE_DOT='╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌'

heavy() { printf '%s\n' "$RULE_HEAVY"; }
light() { printf '%s\n' "$RULE_LIGHT"; }
dotted() { printf '%s\n' "$RULE_DOT"; }

# ------------------------------------------------------------ drive lookup
#
# Resolved once per distinct directory: df forks, and a backfill can hold
# hundreds of calls from a dozen directories.

volume_of() {
    vo_dir=$1
    [ -d "$vo_dir" ] || vo_dir=$(dirname -- "$vo_dir")
    [ -d "$vo_dir" ] || { printf '?\n'; return 0; }
    df -P -- "$vo_dir" 2>/dev/null | awk 'NR==2 { print $6; exit }' || printf '?\n'
}

device_of() {
    do_dir=$1
    [ -d "$do_dir" ] || do_dir=$(dirname -- "$do_dir")
    [ -d "$do_dir" ] || { printf '?\n'; return 0; }
    df -P -- "$do_dir" 2>/dev/null | awk 'NR==2 { print $1; exit }' || printf '?\n'
}

# ------------------------------------------------------------------ header

write_header() {
    {
        heavy
        printf '  lwdbg console log\n'
        printf '  generated %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf '  host %s  user %s  shell %s\n' \
            "$(hostname 2>/dev/null || echo '?')" \
            "$(id -un)" \
            "${SHELL:-?}"
        printf '  pid %s  ppid %s\n' "$$" "${PPID:-?}"
        heavy
        printf '\n'
        printf '  VOLUMES IN PLAY\n'
        light
        df -h 2>/dev/null | awk 'NR==1 || /^\/dev\// { printf "  %s\n", $0 }'
        printf '\n'
        heavy
        printf '  %-24s %7s  %-14s %-4s %-10s %-3s %8s %8s  %-22s\n' \
            TIMESTAMP DUR_MS TOOL VRD PERMISSION SBX OUT_B ERR_B VOLUME
        printf '  %s\n' "CWD / COMMAND"
        heavy
    } > "$OUT"
}

# ---------------------------------------------------------------- backfill

backfill() {
    # Which transcripts belong to this work, and why this is not a one-liner.
    #
    # The project slug is the directory Claude Code was started in. Here that
    # is /Volumes/ARCHIVE/Sites while the repo is
    # /Volumes/ARCHIVE/librewolf-debugger — siblings, so neither contains the
    # other and slug or ancestor matching finds nothing. Grepping every
    # project for the path, the first attempt, silently pulled in an
    # unrelated session and interleaved two projects by timestamp.
    #
    # So the test is: did this session touch the repo at all. That is a fact
    # in the data, the per-directory row counts below make the scope visible,
    # and --project pins it exactly when that is not enough.
    dirs=''
    if [ -n "$ONLY_PROJECT" ]; then
        dirs="$PROJECTS/$ONLY_PROJECT"
        [ -d "$dirs" ] || { printf 'no such project: %s\n' "$dirs" >&2; exit 1; }
        printf '  scope: %s\n' "$ONLY_PROJECT" >&2
    elif [ "$ALL_PROJECTS" = yes ]; then
        for d in "$PROJECTS"/*
        do
            [ -d "$d" ] && dirs="$dirs $d"
        done
        printf '  scope: every project under %s\n' "$PROJECTS" >&2
    else
        for d in "$PROJECTS"/*
        do
            [ -d "$d" ] || continue
            if grep -qlF -- "$REPO" "$d"/*.jsonl 2>/dev/null; then
                dirs="$dirs $d"
            fi
        done
        printf '  scope: sessions that touched %s\n' "$REPO" >&2
        printf '         (--all-projects for everything, --project SLUG to pin)\n' >&2
    fi

    write_header

    n=0
    for d in $dirs
    do
        for f in "$d"/*.jsonl
        do
            [ -f "$f" ] || continue
            before=$(grep -c '^    cmd    ' "$OUT" 2>/dev/null) || before=0

            # One pass: collect tool_use blocks, then tool_result blocks, then
            # join. jq's input_line_number is not needed — the join key is in
            # the data.
            jq -r -s '
              # fromdateiso8601 rejects fractional seconds, and every
              # timestamp here has them. Stripping before parsing is the whole
              # fix; without it the filter throws and — with stderr discarded,
              # as it was on the first attempt — produced a silent zero rows.
              # (No apostrophes in this comment: the whole filter is inside a
              # single-quoted shell string.)
              def epoch: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
              # Every tool_use, with the record fields that matter.
              ( [ .[]
                  | select(.type == "assistant")
                  | . as $r
                  | (.message.content // [])
                  | (if type == "array" then .[] else empty end)
                  | select(type == "object" and .type == "tool_use")
                  | { id: .id,
                      tool: .name,
                      ts: $r.timestamp,
                      cwd: ($r.cwd // "?"),
                      model: ($r.requestedModel // "?"),
                      effort: ($r.effort // "?"),
                      sidechain: ($r.isSidechain // false),
                      req: ($r.requestId // "?"),
                      cmd: ( .input.command
                             // .input.file_path
                             // .input.pattern
                             // .input.url
                             // .input.query
                             // (.input | tostring) ),
                      desc: (.input.description // "") }
                ] ) as $uses
              # Every tool_result, keyed by the id it answers.
              | ( [ .[]
                    | select(.type == "user")
                    | . as $r
                    | (.message.content // [])
                    | (if type == "array" then .[] else empty end)
                    | select(type == "object" and .type == "tool_result")
                    | { id: .tool_use_id,
                        ts: $r.timestamp,
                        err: (.is_error // false),
                        perm: ( ($r.permissionDecision // "-")
                                | if type == "object"
                                  then ((.decision // .behavior // "obj") | tostring)
                                  else tostring end ),
                        sbx: ($r.sandboxed // false),
                        intr: ((($r.toolUseResult // {}) | if type=="object" then (.interrupted // false) else false end)),
                        out: ((($r.toolUseResult // {}) | if type=="object" then ((.stdout // "") | length) else 0 end)),
                        errb: ((($r.toolUseResult // {}) | if type=="object" then ((.stderr // "") | length) else 0 end)) }
                  ] | INDEX(.id) ) as $res
              | $uses[]
              | . as $u
              | ($res[$u.id] // {}) as $r
              | [ $u.ts,
                  ( if ($r.ts // null) != null
                    then ((($r.ts | epoch) - ($u.ts | epoch)) * 1000 | floor | tostring)
                    else "-" end ),
                  $u.tool,
                  ( if ($r.intr // false) then "INTR"
                    elif ($r.err // false) then "ERR"
                    elif ($r.ts // null) == null then "?"
                    else "ok" end ),
                  (($r.perm // "-") | tostring),
                  ( if ($r.sbx // false) then "yes" else "no" end ),
                  (($r.out // 0) | tostring),
                  (($r.errb // 0) | tostring),
                  $u.cwd,
                  $u.model,
                  $u.effort,
                  ( if $u.sidechain then "sub" else "main" end ),
                  $u.req,
                  ($u.desc | gsub("\n"; " ")),
                  ($u.cmd | gsub("\n"; "\u0001"))
                ]
              | @tsv
            ' "$f" |
            while IFS="$(printf '\t')" read -r ts dur tool vrd perm sbx outb errb cwd model effort chain req desc cmd
            do
                vol=$(volume_of "$cwd")
                dev=$(device_of "$cwd")
                {
                    printf '  %-24s %7s  %-14s %-4s %-10s %-3s %8s %8s  %-22s\n' \
                        "$ts" "$dur" "$tool" "$vrd" "$perm" "$sbx" "$outb" "$errb" "$vol"
                    printf '    cwd    %s\n' "$cwd"
                    printf '    drive  %s on %s\n' "$dev" "$vol"
                    printf '    meta   model=%s effort=%s ctx=%s req=%s\n' \
                        "$model" "$effort" "$chain" "$req"
                    [ -n "$desc" ] && printf '    desc   %s\n' "$desc"
                    # \001 stands in for the newlines removed above, so a
                    # multi-line command is still readable as what was run.
                    printf '    cmd    %s\n' "$(printf '%s' "$cmd" | tr '\001' '\n' | sed '2,$s/^/           /')"
                    dotted
                } >> "$OUT"
                n=$((n + 1))
            done
            after=$(grep -c '^    cmd    ' "$OUT" 2>/dev/null) || after=0
            printf '  %5s rows  %s\n' "$((after - before))" "$f" >&2
        done
    done

    {
        printf '\n'
        heavy
        printf '  end of backfill\n'
        heavy
    } >> "$OUT"
    printf 'wrote %s\n' "$OUT"
    total=$(grep -c '^    cmd    ' "$OUT" 2>/dev/null) || total=0
    printf '  %s rows total\n' "$total"
}

# -------------------------------------------------------------------- hook
#
# Called by a Claude Code PostToolUse hook, which hands the event on stdin.
# Everything is read from that JSON, so this works for any tool, not just
# Bash, and does not need to guess what ran.

hook() {
    ev=$(cat)
    tool=$(printf '%s' "$ev" | jq -r '.tool_name // "?"' 2>/dev/null || echo '?')
    cwd=$(printf '%s' "$ev" | jq -r '.cwd // "?"' 2>/dev/null || echo '?')
    sess=$(printf '%s' "$ev" | jq -r '.session_id // "?"' 2>/dev/null || echo '?')
    cmd=$(printf '%s' "$ev" | jq -r '
        .tool_input
        | (.command // .file_path // .pattern // .url // .query // (. | tostring))
        | gsub("\n"; "\u0001")' 2>/dev/null || echo '?')
    desc=$(printf '%s' "$ev" | jq -r '.tool_input.description // ""' 2>/dev/null || echo '')
    outb=$(printf '%s' "$ev" | jq -r '
        (.tool_response // "") | if type == "string" then length
        elif type == "object" then ((.stdout // "") | length) else 0 end' 2>/dev/null || echo 0)
    errb=$(printf '%s' "$ev" | jq -r '
        (.tool_response // {}) | if type == "object" then ((.stderr // "") | length) else 0 end' 2>/dev/null || echo 0)

    [ -s "$OUT" ] || write_header

    vol=$(volume_of "$cwd")
    dev=$(device_of "$cwd")
    free=$(df -h -- "$cwd" 2>/dev/null | awk 'NR==2 { print $4; exit }' || echo '?')

    {
        printf '  %-24s %7s  %-14s %-4s %-10s %-3s %8s %8s  %-22s\n' \
            "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "live" "$tool" "ok" "live" "-" \
            "$outb" "$errb" "$vol"
        printf '    cwd    %s\n' "$cwd"
        printf '    drive  %s on %s (%s free)\n' "$dev" "$vol" "$free"
        printf '    meta   session=%s pid=%s ppid=%s user=%s\n' \
            "$sess" "$$" "${PPID:-?}" "$(id -un)"
        [ -n "$desc" ] && printf '    desc   %s\n' "$desc"
        printf '    cmd    %s\n' "$(printf '%s' "$cmd" | tr '\001' '\n' | sed '2,$s/^/           /')"
        dotted
    } >> "$OUT"
}

# ----------------------------------------------------------- install-hook

install_hook() {
    settings="$REPO/.claude/settings.json"
    mkdir -p -- "$REPO/.claude"
    entry=$(cat <<JSON
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "sh \"\$CLAUDE_PROJECT_DIR/tools/console-log.sh\" hook"
          }
        ]
      }
    ]
  }
}
JSON
)
    if [ -f "$settings" ]; then
        # Merge rather than overwrite: this file may already carry
        # permissions or env the user depends on.
        tmp="$settings.part$$"
        printf '%s' "$entry" | jq -s '.[0] * .[1]' "$settings" - > "$tmp" &&
            mv -f "$tmp" "$settings"
        printf 'merged the hook into %s\n' "$settings"
    else
        printf '%s\n' "$entry" > "$settings"
        printf 'wrote %s\n' "$settings"
    fi
    printf '\nEvery tool call from the next session on appends to:\n  %s\n' "$OUT"
    printf '\nThis session will not log retroactively — the hook is read at\n'
    printf 'startup. Run the backfill mode for what has already happened.\n'
}

# ------------------------------------------------------------------ matrix
#
# Three matrices, because three different questions get asked of a log like
# this: what ran, where it ran, and what it cost.

matrix() {
    [ -s "$OUT" ] || { printf 'no log at %s; run backfill first\n' "$OUT" >&2; exit 1; }

    heavy
    printf '  MATRIX 1 — calls by tool and verdict\n'
    light
    awk '
    /^  [0-9]{4}-/ { tool=$3; vrd=$4; c[tool"\t"vrd]++; tools[tool]; vrds[vrd] }
    END {
        printf "  %-16s", "TOOL"
        n = split("ok ERR INTR ?", order, " ")
        for (i = 1; i <= n; i++) printf "%8s", order[i]
        printf "%8s\n", "TOTAL"
        for (t in tools) {
            tot = 0
            printf "  %-16s", t
            for (i = 1; i <= n; i++) { v = c[t"\t"order[i]] + 0; tot += v; printf "%8d", v }
            printf "%8d\n", tot
        }
    }' "$OUT" | { IFS= read -r hdr; printf '%s\n' "$hdr"; sort -k2 -rn; } | head -40
    printf '\n'

    heavy
    printf '  MATRIX 2 — calls by volume\n'
    printf '  This is the one that explains a hang. Three volumes are in play\n'
    printf '  and their write latency differs by more than an order of\n'
    printf '  magnitude.\n'
    light
    awk '/^  [0-9]{4}-/ { v=$NF; c[v]++ } END { for (k in c) printf "  %-30s %6d\n", k, c[k] }' "$OUT" |
        sort -k2 -rn
    printf '\n'

    heavy
    printf '  MATRIX 3 — duration, by tool\n'
    light
    awk '
    /^  [0-9]{4}-/ && $2 ~ /^[0-9]+$/ {
        t=$3; d=$2+0; n[t]++; s[t]+=d; if (d > mx[t]) mx[t]=d
    }
    END {
        printf "  %-16s %7s %10s %10s %12s\n", "TOOL", "CALLS", "MEAN_MS", "MAX_MS", "TOTAL_S"
        for (t in n) printf "  %-16s %7d %10d %10d %12.1f\n", t, n[t], s[t]/n[t], mx[t], s[t]/1000
    }' "$OUT" | { IFS= read -r hdr; printf '%s\n' "$hdr"; sort -k5 -rn; }
    printf '\n'
    heavy
}

# ------------------------------------------------------------------ drives

drives() {
    heavy
    printf '  DRIVE DETAIL\n'
    heavy
    printf '\n  mounted filesystems\n'
    light
    df -h 2>/dev/null | sed 's/^/  /'
    printf '\n  inode pressure\n'
    light
    df -i 2>/dev/null | sed 's/^/  /'
    printf '\n  paths this project touches\n'
    light
    for p in "$REPO" "$HOME/.cache/lwdbg-target" "$HOME/.local/share/lwdbg" \
             "$HOME/.cargo" "$HOME/.claude/projects" /Volumes
    do
        [ -e "$p" ] || continue
        printf '  %-40s %-14s %s\n' \
            "$p" "$(device_of "$p")" "$(volume_of "$p")"
    done
    printf '\n'
    heavy
}

case "$MODE" in
backfill)     backfill ;;
hook)         hook ;;
install-hook) install_hook ;;
matrix)       matrix ;;
drives)       drives ;;
*)
    printf 'usage: %s [backfill|hook|install-hook|matrix|drives]\n' \
        "$(basename -- "$0")" >&2
    printf '       [--out FILE] [--all-projects]\n' >&2
    exit 2
    ;;
esac
