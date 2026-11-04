# lwdbg — librewolf-debugger

Measure, debug and control browsers through a local caching proxy and a debug
store that correlates every signal under one trace id.

```sh
./configure --prefix=/usr/local
make
make check
make install
```

POSIX `sh` and POSIX `make` throughout. No bash, no zsh, no autoconf — the
target machines include a FreeBSD box where zsh is not in the base install, and
anything a packager touches has to honour `DESTDIR`.

## Why it is shaped this way

There are **three** browser automation protocol families, not one, and a
fourth engine with none at all:

| Engine | Transport | Extension |
| --- | --- | --- |
| Gecko — Firefox, LibreWolf, Waterfox, Floorp, Zen, Mullvad, Tor, IceCat | Marionette, DevTools RDP, WebDriver BiDi. **No CDP.** | WebExtensions |
| Blink — Chrome, Edge, Brave, Opera, Vivaldi | CDP, increasingly BiDi | MV3, different surface |
| WebKit — Safari, GNOME Web, Orion | Remote Inspector, `safaridriver` | Safari Web Extension, Xcode wrapper |
| Goanna — Pale Moon, Basilisk | — | **none** |

So a single extension cannot span engines, and no transport reaches Goanna.
The **proxy** is the only layer that reaches all four, which is why it is the
backbone rather than a convenience. Everything else is a layer above it that
some subset of browsers can use.

## Layers

| Layer | Reaches | Command |
| --- | --- | --- |
| proxy | every engine, including Goanna and Android | `lwdbg proxy`, `lwdbg measure` |
| transports | Gecko, fresh launch | `lwdbg launch`, `lwdbg eval`, `lwdbg measure --via` |
| extension | Gecko, a session **already running** | `lwdbg live` |
| parity | any two renders, GPU or CPU | `lwdbg parity` |
| adb | Android Firefox, real multitouch | `lwdbg android` |
| vantage | other machines over ssh | `lwdbg vantage` |

```sh
lwdbg doctor --json     # what works here, and what is missing
lwdbg detect --json     # installed browsers, engines, usable transports
man lwdbg
```

## The two rules

1. **Three runs, report the median.** The origin this was built against was
   measured swinging from 0.22 s TTFB to a 180 s timeout — a 360x spread. One
   observation is not a property of a server.
2. **Always keep a non-browser baseline.** `lwdbg measure --via curl` is the
   anchor. Last cross-check on this machine: Marionette median TTFB 0.248 s
   against curl's 0.223 s, byte-identical on the wire (1 956 136 bytes). If
   those two disagree by much more than 20%, the browser path is instrumented
   wrong and nothing downstream is trustworthy.

That second rule earned its place. Reading `loadEventEnd` directly reported
**0.714 s** for a load that actually took **6.0 s**, because Gecko anchors
`performance.timeOrigin` at document creation — *after* the response. Every
milestone now gets shifted to the entry's earliest mark, and the anchor used
is reported with each result.

## What is built

```text
configure  Makefile.in           POSIX build system
cli/                             the lwdbg binary: 14 subcommands
proxy/                           MITM caching proxy, local CA, Vary-aware cache
ssot/                            SQLite + content-addressed blobs, one schema
transports/                      Marionette, DevTools RDP, WebDriver BiDi, MOZ_LOG
gpu/                             SSIM + pHash, wgpu shader with a CPU reference
host/                            native messaging host for the live bridge
extension/                       clean-room MV2 WebExtension
skills/librewolf-debugger/       SKILL.md + 13 reference documents
evals/                           13 mechanical cases + 9 trigger prompts
packaging/                       FreeBSD port, Void template, Windows notes
doc/lwdbg.1                      mdoc man page
scripts/                         bundle.sh, setup-ssh.sh
```

Clean-room: no upstream code. 161 tests, including a fake-extension
integration test for the live bridge and a GPU-against-CPU agreement check.

## Safety

- A proxy with a trusted CA sees **all** traffic from any browser pointed at
  it, credentials included. Scope it: `lwdbg proxy start --only-host <host>`.
  `example.com` covers `cdn.example.com` and deliberately **not**
  `notexample.com`; there is a test for that.
- **Tor Browser is refused** unless `--force-tor`. Local interception breaks
  its threat model.
- `lwdbg live` acts on the user's real session. The host refuses a
  state-changing command without `--allow-write`, and the refusal happens
  *before* anything reaches the browser. That is a mechanism, not a substitute
  for asking first.
- `lwdbg launch` writes `devtools.debugger.prompt-connection=false` and
  `privacy.resistFingerprinting=false`, so it refuses any profile directory it
  did not create.
- Remove the CA when done: `lwdbg proxy ca-uninstall --profile <dir>`.

## Known blockers, all measured here

- **DevTools RDP does not work on LibreWolf.** `librewolf.cfg` forces
  `devtools.debugger.remote-enabled` false with a plain `pref()`, and
  AutoConfig is applied *after* `user.js`, so no user pref can override it.
  `--start-debugger-server` is accepted and silently does nothing. Marionette
  and BiDi both work. Combined with those two being startup-only, an
  already-running LibreWolf is reachable only through the extension or the
  proxy.
- **`privacy.resistFingerprinting` silently falsifies every measurement** —
  resource timing comes back empty, `responseStart` negative.
- **A mistyped pref in `user.js` aborts the rest of the file**, with no error
  and no transport. So do **stray browser instances**: ten leftovers made every
  launch fail to open Marionette, with a benign GFX warning as the only output.

`skills/librewolf-debugger/references/` has the derivations, including the
Gecko source that proves the RDP gate.

## Two binaries

`lwdbg` links **no wgpu and no naga** — verified, not asserted:
`cargo tree -p lwdbg | grep -c -E 'wgpu|naga'` is 0, and 12 for `lwdbg-gpu`.

The GPU and shader work is a second binary, built only when
`./configure --with-gpu` asks for it. The reason is measured: linking wgpu
into `lwdbg` under thin LTO stalled this 8 GiB machine with rustc at 0% CPU
and swap at 88%, and the link never finished. Cargo unifies features within
one invocation, so the Makefile runs **two separate ones** — that is the only
thing that makes the split real.

`lwdbg parity --backend gpu` delegates to the helper; `--backend auto` falls
back to the CPU reference when it is absent; `--backend gpu` without it
**errors** rather than quietly giving you the CPU, because asking for the GPU
and not getting it is how a benchmark becomes fiction.

```sh
lwdbg-gpu backends                  # adapter, and which shader compilers exist
lwdbg-gpu emit --out audit/shaders  # one WGSL source -> five languages
lwdbg-gpu verify                    # compile each with the platform's compiler
```

`gpu/shaders/ssim.wgsl` is the only hand-written shader. SPIR-V, MSL, HLSL and
GLSL are translated from it, and `verify` compiles each with the real
compiler — a language whose compiler is absent is **skipped with its reason**,
never reported as passing. On this machine SPIR-V and GLSL are accepted with
no diagnostics; MSL skips because the Metal compiler is a separate Xcode
component, and HLSL because `dxc` has no macOS build.

`gpu/js/parity.mjs` does the same comparison inside a page, and agrees with
the Rust reference **bit for bit** — which required sharing the DCT cosine
table, because V8 bundles its own fdlibm for `Math.cos` and the last-bit
difference flipped 3 of 64 hash bits. Details in
`skills/librewolf-debugger/references/shaders.md`.

## Release gate

```sh
make preflight      # catalogue, lint, audit, SARIF, dup, rules, sbom, tests
```

One command, one verdict, and every skip printed with its reason. The
catalogue distinguishes **usable**, **not installed** and **nothing to scan** —
19/11/5 here — because a green tick for a Python linter in a repository with
no Python looks like coverage and is not.

Current state: clippy 0, `cargo deny` clean on all four checks, `cargo
machete` clean, gitleaks clean, 0 open SARIF findings with 10 suppressed and
each suppression's reason written at the site. `cargo geiger` reports
**0/0 unsafe** for both `lwdbg-proxy` and `lwdbg-ssot` — the crates that
terminate TLS and parse attacker-influenced bytes. The whole workspace has one
`unsafe` block, a `kill(2)` call, with a SAFETY comment.

Three real security findings came out of it and were fixed, not suppressed:
browser profiles and the CA certificate were being written to predictable
paths in the shared temp directory, and `rustls-pemfile` (unmaintained,
RUSTSEC-2025-0134) was a direct dependency in the TLS path. Details and the
rest in `skills/librewolf-debugger/references/auditing.md`.

## Other agent harnesses

One canonical context source, native entry points generated for each:

```sh
lwdbg adapters list
lwdbg adapters install --all
lwdbg adapters emit --harness agents --out /path/to/AGENTS.md
```

Claude Code, Kilo Code and Codex get a **symlink** to the canonical skill
directory, so there is no copy to go stale. opencode and Kilo get agent
markdown, Gemini CLI and Antigravity an extension, Copilot and anything else a
delimited `AGENTS.md` block that leaves hand-written text alone. A path that
was inferred rather than observed is reported as `INFERRED`.

## Licence

MIT or Apache-2.0, at your option.
