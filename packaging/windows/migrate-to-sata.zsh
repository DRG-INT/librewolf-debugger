#!/usr/bin/env zsh
# Move space-hungry directories off C: onto C:\Sata(0x01)\ and leave a link
# behind. For cmder's zsh.
#
#   ./migrate-to-sata.zsh survey              # look, change nothing
#   ./migrate-to-sata.zsh migrate --dry-run   # print every command
#   ./migrate-to-sata.zsh migrate             # do it
#
# Run `survey` and read it before anything else. It answers the question that
# decides the whole job: for each candidate, how big is it, does the
# destination already hold a copy, and do the two differ.
#
# ---------------------------------------------------------------------------
# The three things that make this go wrong on Windows
#
# 1. `ln -s` in cmder/MSYS2 **silently copies** unless MSYS is configured.
#    The default `winsymlinks` behaviour is to make a copy, so a 40 GB cache
#    "linked" that way uses 80 GB and the two halves drift apart. This script
#    exports MSYS=winsymlinks:nativestrict, which makes ln -s either create a
#    real symlink or fail loudly. It never silently copies.
#
# 2. A native symlink needs Developer Mode or an elevated shell; a **directory
#    junction** needs neither. Junctions are also followed by every toolchain,
#    while some refuse to traverse a symlink. So: junctions for directories,
#    ln -s only for single files.
#
# 3. For a cache with a documented environment variable, **redirect instead of
#    linking**. CARGO_HOME is supported; a junction under ~/.cargo merely
#    works until something checks for a reparse point. The script emits setx
#    lines for those and links only what has no variable.
#
# Nothing is deleted until the copy has been verified, and nothing is touched
# while a process that owns it is running.

set -u
setopt pipe_fail

# Make ln -s honest. Without this the "links" below would be copies.
export MSYS=winsymlinks:nativestrict
export MSYS2_ARG_CONV_EXCL='*'

DEST_WIN='C:\Sata(0x01)'
DEST='/c/Sata(0x01)'
USERDIR="/c/Users/LaborPC"

MODE=${1:-survey}
DRY=no
[[ "${2:-}" == "--dry-run" ]] && DRY=yes

# name | path under the user directory | env var that redirects it | process
# that must not be running. An empty env var means it has to be linked.
typeset -a CANDIDATES
CANDIDATES=(
  'cargo|.cargo|CARGO_HOME|cargo'
  'rustup|.rustup|RUSTUP_HOME|rustc'
  'npm-cache|AppData/Local/npm-cache|npm_config_cache|node'
  'pnpm|AppData/Local/pnpm|PNPM_HOME|node'
  'pip-cache|AppData/Local/pip/Cache|PIP_CACHE_DIR|python'
  'gradle|.gradle|GRADLE_USER_HOME|java'
  'nuget|.nuget/packages|NUGET_PACKAGES|dotnet'
  'dotnet|.dotnet|DOTNET_CLI_HOME|dotnet'
  'go|go/pkg/mod|GOMODCACHE|go'
  'gopath|go|GOPATH|go'
  'gradle-caches|AppData/Local/Gradle|GRADLE_USER_HOME|java'
  'android-sdk|AppData/Local/Android/Sdk||adb'
  'android-avd|.android/avd||qemu-system'
  'docker|AppData/Local/Docker||docker'
  'vscode-ext|.vscode/extensions||Code'
  'nvm|AppData/Roaming/nvm||node'
  'conda-pkgs|.conda/pkgs||python'
  'huggingface|.cache/huggingface||python'
  'torch|.cache/torch||python'
  'ollama|.ollama/models||ollama'
)

say() { print -r -- "$*" }
run() {
  if [[ "$DRY" == yes ]]; then
    print -r -- "    would run: $*"
  else
    print -r -- "    + $*"
    "$@"
  fi
}

human() {
  # du on a large tree is slow; this is the number that decides whether a
  # candidate is worth moving, so it is worth the wait.
  du -sh -- "$1" 2>/dev/null | awk '{print $1}'
}

process_running() {
  [[ -z "$1" ]] && return 1
  tasklist 2>/dev/null | grep -qi "^$1" && return 0
  return 1
}

# ------------------------------------------------------------------ survey

survey() {
  say "Survey of $USERDIR -> $DEST_WIN"
  say ""
  printf '%-16s %-8s %-30s %s\n' NAME SIZE DESTINATION STATE
  print -r -- "--------------------------------------------------------------------------------"

  local entry name rel var proc src dst size state
  for entry in $CANDIDATES
  do
    name=${entry%%|*}
    rel=${${entry#*|}%%|*}
    var=${${${entry#*|}#*|}%%|*}
    proc=${entry##*|}
    src="$USERDIR/$rel"

    [[ -e "$src" ]] || continue
    # Already a link: nothing to move.
    if [[ -L "$src" ]]; then
      printf '%-16s %-8s %-30s %s\n' "$name" "-" "$(readlink -- "$src")" "already linked"
      continue
    fi

    dst="$DEST/$rel"
    size=$(human "$src")

    if [[ ! -e "$dst" ]]; then
      state="destination absent — clean move"
    else
      local sc dc
      sc=$(find "$src" -type f 2>/dev/null | wc -l | tr -d ' ')
      dc=$(find "$dst" -type f 2>/dev/null | wc -l | tr -d ' ')
      if [[ "$sc" == "$dc" ]]; then
        state="destination exists, same file count ($sc) — verify, then relink"
      else
        state="DESTINATION DIFFERS src=$sc dst=$dc files — DO NOT overwrite"
      fi
    fi

    if process_running "$proc"; then
      state="$state; $proc IS RUNNING"
    fi
    if [[ -n "$var" ]]; then
      state="$state; has \$$var"
    fi

    printf '%-16s %-8s %-30s %s\n' "$name" "$size" "${dst#/c/}" "$state"
  done

  say ""
  say "Reading this table:"
  say "  * 'destination absent' is the easy case."
  say "  * 'DESTINATION DIFFERS' means two divergent copies already exist."
  say "    Decide which is authoritative yourself; this script will not"
  say "    overwrite either."
  say "  * 'has \$VAR' means the supported fix is an environment variable, not"
  say "    a link. 'migrate' emits the setx line for those."
  say "  * 'IS RUNNING' blocks the move. A junction swapped under an open"
  say "    handle loses writes."
  say ""
  say "Free space:"
  df -h /c "$DEST" 2>/dev/null | sed 's/^/  /'
}

# ----------------------------------------------------------------- migrate

migrate_one() {
  local name=$1 rel=$2 var=$3 proc=$4
  local src="$USERDIR/$rel" dst="$DEST/$rel"

  [[ -e "$src" ]] || return 0
  [[ -L "$src" ]] && { say "  $name: already a link, skipping"; return 0 }

  if process_running "$proc"; then
    say "  $name: SKIPPED, $proc is running"
    return 0
  fi

  say "  $name: $src -> $dst"

  if [[ -e "$dst" ]]; then
    local sc dc
    sc=$(find "$src" -type f 2>/dev/null | wc -l | tr -d ' ')
    dc=$(find "$dst" -type f 2>/dev/null | wc -l | tr -d ' ')
    if [[ "$sc" != "$dc" ]]; then
      say "    REFUSED: destination has $dc files, source has $sc."
      say "    Two divergent copies. Resolve by hand."
      return 0
    fi
    say "    destination already matches ($sc files); skipping the copy"
  else
    run mkdir -p -- "${dst:h}"
    # robocopy, not move: it is restartable, preserves ACLs and timestamps,
    # and reports what it did. /MIR mirrors, /COPYALL keeps the security
    # descriptors, /DCOPY:DAT keeps directory timestamps, /R:1 /W:1 stops it
    # retrying a locked file for thirty seconds each.
    run robocopy "$(cygpath -w -- "$src")" "$(cygpath -w -- "$dst")" \
        /MIR /COPYALL /DCOPY:DAT /R:1 /W:1 /NFL /NDL /NJH
    # robocopy exits 0-7 for success; 8 and above is a real failure.
    local rc=$?
    if [[ "$DRY" == no && $rc -ge 8 ]]; then
      say "    robocopy failed with $rc; source left untouched"
      return 1
    fi
    # Verify before deleting anything.
    if [[ "$DRY" == no ]]; then
      local sc2 dc2
      sc2=$(find "$src" -type f 2>/dev/null | wc -l | tr -d ' ')
      dc2=$(find "$dst" -type f 2>/dev/null | wc -l | tr -d ' ')
      if [[ "$sc2" != "$dc2" ]]; then
        say "    VERIFY FAILED: $sc2 files at source, $dc2 at destination."
        say "    Source left in place. Nothing was deleted."
        return 1
      fi
      say "    verified: $dc2 files"
    fi
  fi

  if [[ -n "$var" ]]; then
    # The supported mechanism. Setting the variable means the tool writes to
    # the new location directly, with no reparse point for it to trip over.
    say "    redirect with the environment variable, not a link:"
    run setx "$var" "$(cygpath -w -- "$dst")"
    say "    then remove the old directory yourself once a run has confirmed"
    say "    the new location is in use:  rm -rf '$src'"
    return 0
  fi

  # No variable: link it. A directory gets a junction, which needs no
  # privilege and which every toolchain follows.
  run rm -rf -- "$src"
  if [[ -d "$dst" ]]; then
    run cmd //c mklink //J "$(cygpath -w -- "$src")" "$(cygpath -w -- "$dst")"
  else
    # A single file can use ln -s; nativestrict makes it a real symlink or an
    # error, never a copy.
    run ln -s -- "$dst" "$src"
  fi
}

migrate() {
  say "Migrating to $DEST_WIN (dry-run=$DRY)"
  say ""
  if [[ ! -d "$DEST" ]]; then
    say "ERROR: $DEST_WIN is not mounted or does not exist."
    say "Check the drive letter and that cmder sees it as $DEST."
    exit 1
  fi

  local entry
  for entry in $CANDIDATES
  do
    migrate_one "${entry%%|*}" "${${entry#*|}%%|*}" \
                "${${${entry#*|}#*|}%%|*}" "${entry##*|}"
  done

  say ""
  say "Done. Open a NEW shell before testing: setx only affects"
  say "processes started after it."
  say ""
  say "Then confirm each tool uses the new location, for example:"
  say "  cargo --version && echo \$CARGO_HOME"
  say "  go env GOMODCACHE"
  say "  npm config get cache"
}

case "$MODE" in
survey)  survey ;;
migrate) migrate ;;
*)
  print -ru2 -- "usage: ${0:t} [survey|migrate] [--dry-run]"
  print -ru2 -- ""
  print -ru2 -- "Run survey first and read it. migrate --dry-run prints every"
  print -ru2 -- "command without running any of them."
  exit 2
  ;;
esac
