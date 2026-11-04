---
name: librewolf-debugger
description: Measure, debug and control browsers through a local caching proxy and a correlated debug store. Use for page load timing (TTFB, DOMContentLoaded, resource timing), whether compression actually applied (wire vs decoded bytes), network capture, cross-browser render parity, controlling an already-running browser session, Firefox/LibreWolf remote debugging, and Android Firefox over adb. Works with every engine via the proxy; Gecko forks additionally get Marionette/RDP/BiDi and extension control. Not a web search tool and not a scraper.
---

`lwdbg` has two things no single-browser tool has: a **proxy** every engine can
use, and an **SSoT** that correlates every signal under one trace ID.

## Decision tree — pick the layer, then read its reference

| You need | Layer | Command | Reference |
| --- | --- | --- | --- |
| Timing, network capture, cache, any browser | **proxy** | `lwdbg proxy`, `lwdbg measure` | `references/proxy.md` |
| Page timing from inside a real browser | **transport** | `lwdbg measure --via marionette\|bidi` | `references/transports.md` |
| Start a browser with transports on | **transport** | `lwdbg launch` | `references/transports.md` |
| Run JavaScript in that browser | **transport** | `lwdbg eval` | `references/transports.md` |
| Inspect a transport that is already listening | **transport** | `lwdbg attach` | `references/transports.md` |
| Control a browser **already running** | **extension** | `lwdbg live` (not built yet) | `references/extension.md` |
| Render parity across browsers | **parity** | `lwdbg parity` | `references/parity.md` |
| Shaders, GPU backends, the JS fallback | **gpu** | `lwdbg-gpu` | `references/shaders.md` |
| Android Firefox | **adb** | `lwdbg android` | `references/android.md` |
| Same measurement from other machines | **vantage** | `lwdbg vantage` | `references/vantage-hosts.md` |
| Which browsers are on this machine | — | `lwdbg detect` | `references/engines.md` |
| Isolated / headless / separate-user runs | — | `lwdbg launch` | `references/isolation.md` |
| Everything the logs can give | — | `lwdbg logs` | `references/logging.md` |
| Wire this tool into another agent CLI | — | `lwdbg adapters` | `references/harnesses.md` |
| Build, install or package this tool | — | `./configure && make` | `references/building.md` |
| Hold it to a release standard | — | `make preflight` | `references/auditing.md` |

Run `lwdbg <command> -h` for exact syntax. **The CLI help is the source of
truth**; this file is the map, not the manual. Pass `--json` whenever another
program or agent consumes the output.

## Start here

```bash
lwdbg doctor --json          # what works on this machine, and what is missing
lwdbg detect --json          # installed browsers, engines, available transports
```

`doctor` is the first call in any unfamiliar environment. It reports missing
dependencies instead of letting a later command fail obscurely.

## The two rules that prevent most failures

1. **Three runs, report the median.** Origins vary by several hundred percent
   run to run. One measurement proves nothing. `lwdbg measure` defaults to 3
   and reports the median; do not override to 1 and draw a conclusion.
2. **Always have a non-browser baseline.** `lwdbg measure --baseline curl` runs
   the same target through curl. If the browser number and the curl number
   disagree by more than ~20%, the browser path is instrumented wrong — fix
   that before interpreting anything.

## Engine reality (why this tool is shaped this way)

There are **three** automation protocol families, not one:

| Engine | Transport | Extension |
| --- | --- | --- |
| Gecko (Firefox, LibreWolf, Waterfox, Floorp, Zen, Mullvad, Tor, IceCat) | Marionette, DevTools RDP, WebDriver BiDi. **No CDP.** RDP is unavailable on LibreWolf specifically. | WebExtensions |
| Blink (Chrome, Edge, Brave, Opera, Vivaldi) | CDP, increasingly BiDi | WebExtensions MV3, different surface |
| WebKit (Safari, GNOME Web, Orion) | WebKit Remote Inspector, `safaridriver` | Safari Web Extension, Xcode wrapper |
| Goanna (Pale Moon, Basilisk) | — | **None.** Proxy-only. |

This is why the proxy is the backbone: it is the only layer that reaches all of
them, including Goanna where no extension can go. Read `references/engines.md`
before assuming a browser supports a transport.

## Safety

- A proxy with a trusted CA sees **all** traffic from any browser configured to
  use it, including credentials and session cookies. Proxy only what the task
  needs: `lwdbg proxy --only-host <host>` restricts capture. Never leave a
  capture-all proxy running after the task.
- `lwdbg live` acts on the user's **real** session with their logins and
  cookies. Never perform payments, transfers, destructive or irreversible
  actions, and never exfiltrate personal data. Ask for explicit approval before
  each such action.
- Starting a debug transport on a browser exposes it to anything on loopback.
  `lwdbg attach` sets `devtools.debugger.force-local` and does not widen it.
- Never disable `devtools.debugger.prompt-connection` on a profile the user
  browses with daily; use a dedicated profile instead (`references/isolation.md`).

## Known blockers — all measured on this machine

- **DevTools RDP does not work on LibreWolf.** `librewolf.cfg` forces
  `devtools.debugger.remote-enabled` false with plain `pref()`, and AutoConfig
  is applied *after* `user.js`, so a user pref cannot override it.
  `--start-debugger-server` is then accepted and silently does nothing. Use
  Marionette or BiDi, both of which work. Full derivation in
  `references/librewolf.md`.
- Marionette and BiDi **cannot be added to a running process** — startup flags
  only. Combined with the line above, an already-running *LibreWolf* cannot be
  attached to by any transport; the routes are the extension layer or the
  proxy.
- **`privacy.resistFingerprinting` silently falsifies every measurement.**
  With it on, resource timing comes back empty and `responseStart` comes back
  negative. `lwdbg launch` disables it in its own scratch profile and nowhere
  else.
- **Navigation Timing is anchored after the response.** Reading `loadEventEnd`
  directly under-reported a 6.0 s load as 0.714 s. `lwdbg` shifts every
  milestone to the entry's earliest mark and reports the anchor it used.
- **A mistyped pref in `user.js` aborts the rest of the file**, with no error
  and no transport. **Stray browser instances** do the same. Kill leftovers
  before measuring.
- `devtools.debugger.prompt-connection` defaults to **true** and raises a
  **modal dialog** on an incoming connection, freezing the browser to further
  automation. `lwdbg launch` writes it false into its own profile only.
- A browser behind the proxy needs the `lwdbg` CA trusted. Gecko forks keep a
  **per-profile** `cert9.db`, not the system keychain. `lwdbg proxy ca-install`
  handles it; `references/proxy.md` explains the per-fork specifics.
