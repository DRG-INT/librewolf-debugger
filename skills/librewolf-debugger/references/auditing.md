# The release gate

```sh
make preflight          # one command, one verdict
```

Everything below also runs on its own. `preflight` runs all of it, prints every
skip with its reason, and writes `audit/SUMMARY.md`.

| Target | What it does |
| --- | --- |
| `catalog` | which analysers exist here and which have anything to scan |
| `lint` | fmt, clippy, shellcheck, mandoc, markdownlint, codespell, actionlint, eslint |
| `audit` | advisories, licences, unsafe, unused deps, secrets |
| `sarif` | merge every tool's SARIF into `audit/report.sarif` |
| `dup` | duplicated blocks — a report, not a gate |
| `audit-rules` | artifact rules; `AUDIT_FIX=1` applies the safe fixes |
| `sbom` | CycloneDX from `Cargo.lock` |

## The catalogue distinguishes three things, not two

`audit/catalog.json` gives every tool a `status` of `ok`, `not-installed`, or
`nothing-to-scan`. That third value is the point.

A report showing a green tick for a Python linter in a repository with no
Python looks like coverage and is not. This repository is 29 `.rs`, 1 `.wgsl`,
1 `.js`, 15 `.md`, 5 `.sh`, 1 mdoc page, 11 `.toml`, 1 `.sql` — and no Python,
no TypeScript, no Dockerfile. So `bandit` and `hadolint` are catalogued as
`nothing-to-scan`, which is honest, rather than as passing.

Measured here: **19 usable, 11 not installed, 5 with nothing to scan.**

## Suppressions carry their reason, at the site

`tools/sarif-merge.sh` counts **open** and **suppressed** findings separately
and gates only on open ones. A suppressed finding is one somebody has already
judged, and the judgement is written where the code is:

```rust
// Test helper. The name carries the pid and the directory is removed first;
// nothing secret is written here, and a test that could not use a temp dir
// would need a fixture in the repo.
// nosemgrep: rust.lang.security.temp-dir.temp-dir
let d = std::env::temp_dir().join(...);
```

Two mechanics that cost an hour to find:

- semgrep honours `nosemgrep` on the finding's line **or the line immediately
  before it**, so the directive must be last in a comment block, with the
  reasoning above it.
- the rule id in the comment must be the full one semgrep reports, which
  repeats the last segment: `rust.lang.security.temp-dir.temp-dir`.

## What the audit found, and what came of it

Three genuine security findings, all fixed rather than suppressed:

1. **Browser profiles in the shared temp directory.** `profile_root` was
   `/tmp/lwdbg-profiles` — a fixed, predictable name in a world-writable
   directory, holding the measured page's cookies and a `user.js` that
   disables the connection prompt. A local attacker could read all of it or
   plant prefs into a browser about to launch. Now under the store directory
   with an explicit `0700`.
2. **The CA certificate handed to `certutil`.** Written to
   `/tmp/lwdbg-ca-<pid>.pem`; a pre-created symlink there would make that
   write clobber an arbitrary file with the user's privileges. Now written
   inside the CA directory.
3. **`rustls-pemfile`** (RUSTSEC-2025-0134, unmaintained) was a direct
   dependency in the TLS path. Replaced with `rustls-pki-types`, which rustls
   already pulls in — one fewer crate, not one more.

And a set of hygiene results:

- **clippy**: 15 findings, now 0. The non-mechanical one was a ten-field
  tuple that became a named `CacheRow`.
- **8 unused dependencies removed** (`time`, `tracing`, `hex`, `serde`,
  `serde_json` across four crates). `cargo machete` reports clean.
- **cargo deny**: `advisories ok, bans ok, licenses ok, sources ok`. Getting
  there needed `deny.toml` to state the licence policy explicitly, with named
  exceptions for `ring` and `webpki-roots` whose expressions are not clean
  SPDX, and it exposed a real packaging defect: the internal crates had a path
  but no version, so the workspace could not have been published.
- **cargo geiger**: `lwdbg-proxy 0/0` and `lwdbg-ssot 0/0`. Zero unsafe in the
  crates that terminate TLS and parse attacker-influenced bytes. The whole
  workspace has exactly one `unsafe` block, a `kill(2)` call, with a SAFETY
  comment stating its obligations.
- **gitleaks**: clean, after a config. In `dir` mode it does not read
  `.gitignore`, so it walked `target/` and found "PRIVATE KEY" compiled into
  the `pem` crate's `.rmeta` — a crate's own test data in a build artifact.
  The config excludes build output and adds one rule for the shape that
  actually leaked here: a password pasted in prose as `pass:VALUE`. That rule
  is tested in both directions, because a secrets rule that never fires is
  indistinguishable from one that works.

## The artifact rules

`tools/audit_rules.sh` is a rule table per artifact kind, in the shape of the
`run_5000_*_audit.py` pattern: audit, auto-fix what is safe, re-audit until
clean. Rules marked no-autofix are reported for a person; nothing here
rewrites a shader's maths.

| Artifact | Rules |
| --- | --- |
| `*.wgsl` | every `@binding` declared in the host layout; workgroup size matches the dispatch rounding; a compute entry point has a bounds guard; no `textureSample` |
| `extension/*.js` | no `eval`/`Function`/`innerHTML`; no network API; every command declares `mutates`; `browser.*` not `chrome.*` |
| `skills/**/*.md` | every reference named in `SKILL.md` exists; no orphan reference; `SKILL.md` under its always-loaded line budget |
| `*.sh`, `configure` | no bashisms; `set -eu`; `#!/bin/sh` |
| `Makefile.in` | no GNU-only construct; no `install(1)`; `DESTDIR` honoured |
| `*.rs` | every `unsafe` block has a SAFETY comment |

Its own first run produced four false positives, each of which was a bug in
the rule rather than in the code, and each is now documented where the rule
is:

- clippy had rewritten the dispatch rounding to `div_ceil`, which the rule did
  not recognise;
- counting `mutates:` caught the extension's own catalogue as well as its
  command table;
- `[[:space:]]` in a `sed` expression is a POSIX character class, not a bash
  test — and the audit script necessarily contains every pattern it searches
  for, so it excludes itself;
- in ERE, `[^#]*?=` parses as `([^#]*)?=` and matches any line with an `=`.
  The `?` has to be escaped.

Two real findings came out of the same run: `building.md` was orphaned and
would never have been loaded, and the SAFETY comment had a suppression note
wedged between it and its `unsafe` block.

## Duplication is a report, not a gate

`tools/dup.sh` windows eight normalised lines and reports exact repeats. It
skips windows that are mostly `field: value,` lines, because a table of struct
literals necessarily repeats its field names — `detect.rs` lists 22 browser
bundles and that is data, not copy-paste.

It found two real duplications, both fixed: the RDP console-actor resolution
written twice in the CLI, now `Rdp::active_console()` in the transport where
the protocol knowledge belongs; and three identical seven-argument `live_print`
calls, now a `live_read` helper with `allow_write` hard-coded false so a
refactor cannot let a mutating command through.

What remains in its output is a test fixture shared by two fake servers, kept
deliberately so each test module stays self-contained.

## What belongs in a gate, and what does not

This took three iterations to get right, and the principle is worth more than
the individual rules:

**A gate must fail only for defects.** Anything that can fail for an unrelated
reason — a missing tool, a loaded machine, a slow origin — belongs outside it,
or must be stated as a skip with its reason.

Applied here:

| Not in `preflight` | Why |
| --- | --- |
| `dup` | some duplication is correct; the judgement is a human's |
| `evals-browser` | launching a browser and timing a request needs an idle machine |
| `console-log` | a record, not a check |
| `cargo geiger` (by default) | minutes of rebuild for an answer that is stable; reused unless `AUDIT_GEIGER=1` |

The browser evals earned their exclusion the hard way. Run inside the gate on
this 8 GiB machine, while cargo was compiling wgpu with swap at 90%:

- Marionette **did not bind within 90 seconds** for two of three launches,
  where an idle machine takes 10–20;
- a TTFB came out at **587 ms against curl's 20 ms** — a 29x ratio produced
  entirely by CPU contention.

Both would have been red. Neither was a defect. A gate that goes red for
machine load is as useless as one that goes green for a missing tool, and the
second failure is the one everybody worries about while the first is the one
that gets the gate switched off.

Two other things changed shape for the same reason:

- the timing cross-check became **directional**. A two-sided ratio is only
  meaningful on an idle machine. The bug class worth catching is
  *under*-reporting — the `timeOrigin` defect read a 6.0 s load as 0.714 s,
  and a browser cannot legitimately be faster than the wire. Being slower
  under load is expected and is now reported as a note.
- it asserts **one** successful run, not two. The three-runs-and-a-median rule
  is about what gets reported to a person; this case asks whether the
  instrumentation is right, and one run answers that. `lwdbg measure` still
  defaults to three and still refuses to call one reportable.

## The command log

```sh
make console-log           # backfill every past call, then the matrices
make console-log-hook      # wire the hook so future calls log themselves
make console-log-drives    # volumes, devices, inode pressure
```

`audit/console.log` holds every tool call this project has made: timestamp,
duration, tool, verdict, permission decision, sandbox flag, stdout/stderr byte
counts, the volume and device the working directory sat on, the model and
effort in play, the request id, and the command itself with its line breaks
intact. Box-drawing dividers rather than dashes, because the rows contain
dashes and pipes and a divider that can be confused with content is not one.

Measured on this project: **1424 rows** — 1305 Bash calls of which 59 errored,
and 6.7 hours of wall-clock command time with a single 603-second maximum.
Split by volume: 859 calls on the internal disk, 522 on `/Volumes/ARCHIVE`.
That second number is the one worth having, because the latency difference
between those two volumes has already explained two apparent hangs.

### Where the past comes from

Claude Code writes a JSONL transcript per session under
`~/.claude/projects/<slug>/<uuid>.jsonl`. A tool invocation is a `tool_use`
block on an `assistant` record; its outcome is a `tool_result` block on the
following `user` record, joined by `tool_use_id`. That join is the only place a
duration, an exit state or an output size exists — neither record has them
alone.

Four things cost real time getting this right, each now a comment at the site:

- **`fromdateiso8601` rejects fractional seconds**, and every timestamp has
  them. With stderr discarded the filter threw and produced a silent zero
  rows, which is exactly the failure mode this project keeps arguing against.
  The `2>/dev/null` is gone.
- **`message.content` is sometimes a string**, not an array, and iterating a
  string aborts the whole filter.
- **`permissionDecision` is sometimes an object**, and `@tsv` refuses one.
- **`grep -c` prints `0` *and* exits 1** when there is no match, so
  `$(grep -c … || echo 0)` yields `0\n0` and the arithmetic fails.

### Scope is explicit, and visible

The project slug is the directory Claude Code was started in. Here that is
`/Volumes/ARCHIVE/Sites` while the repo is
`/Volumes/ARCHIVE/librewolf-debugger` — siblings, so neither contains the
other and slug or ancestor matching finds nothing.

Grepping every project for the path instead, the first attempt, silently
interleaved an unrelated session by timestamp. So the test is whether a
session touched the repo at all, the **per-transcript row counts are printed**
so the scope is visible rather than assumed, and `--project SLUG` pins it when
that is not enough. `--all-projects` widens it deliberately.

### Future calls

`console-log-hook` writes a `PostToolUse` hook into `.claude/settings.json`
using `$CLAUDE_PROJECT_DIR` rather than an absolute path, because that file is
committed and an absolute path would be wrong on another machine. The hook is
read at startup, so it takes effect from the next session; the backfill covers
everything before that.

The log is under `audit/`, which `.gitignore` excludes — it is a record of this
machine's sessions, not project content.

## CI runs the same gate

`.github/workflows/ci.yml` runs `make preflight`, so CI and a local run cannot
drift apart. It also does the two things a developer's machine cannot:

- the **staged install round trip** — install to a `DESTDIR`, diff the result
  against `packaging/MANIFEST`, uninstall, assert the tree is empty;
- **`dash -n`** on every script. On macOS `/bin/sh` is bash in disguise, so
  that job is the only place the POSIX claim is actually tested.
