# LibreWolf specifics

All of this was **measured** against **LibreWolf 153.0.1-1 arm64** (buildID
20260728224209) on macOS 25.5.0, 2026-11-03. Where a claim came from reading
Gecko's own source, the file is named.

## Paths

`/Applications/LibreWolf.app` is a **symlink** to
`/Volumes/ARCHIVE/APPLICATIONS_OFFLOAD/LibreWolf.app`. Resolve symlinks before
reporting an install location.

There is also a **second bundle** at
`/Volumes/_ARCHIVE/Applications/LibreWolf.app` (note the leading underscore —
a different volume), and that is the one running the user's day-to-day
profile `swkzn2k7.default-default-1`. Two bundles of the same application
means "is LibreWolf running?" has to be answered per bundle path, not per
process name.

- Binary: `<bundle>/Contents/MacOS/librewolf`. The `.app` directory is not
  itself executable; invoking it fails in a way that reads like a missing
  install.
- Bundle ID: `net.librewolf.librewolf`
- Config: `<bundle>/Contents/Resources/librewolf.cfg`
- Policies: `<bundle>/Contents/Resources/distribution/policies.json`

## The rule that decides everything: `pref()` beats user.js, `defaultPref()` does not

`librewolf.cfg` is an AutoConfig (MCD) file, and AutoConfig is applied **after**
`prefs.js`/`user.js`. So:

| In librewolf.cfg | Effect | Can user.js override it? |
| --- | --- | --- |
| `pref("x", v)` | sets the *user* value at startup, after user.js was read | **No** |
| `defaultPref("x", v)` | sets the *default* value | **Yes** |
| `lockPref("x", v)` | locks it | No |

Verified by launching with a `user.js` that set both and then reading the
resulting `prefs.js`:

```text
librewolf.cfg: pref("devtools.debugger.remote-enabled", false);
user.js:       user_pref("devtools.debugger.remote-enabled", true);
prefs.js:      user_pref("devtools.debugger.remote-enabled", false);   <-- cfg won

librewolf.cfg: defaultPref("privacy.resistFingerprinting", true);
user.js:       user_pref("privacy.resistFingerprinting", false);
prefs.js:      user_pref("privacy.resistFingerprinting", false);       <-- user.js won
```

## Transport availability — measured, not assumed

| Transport | Flag | On LibreWolf 153 |
| --- | --- | --- |
| Marionette | `--marionette` | **works**, headless and headed |
| WebDriver BiDi | `--remote-debugging-port N` | **works** |
| DevTools RDP | `--start-debugger-server N` | **does not work** |

RDP fails for a specific, non-negotiable reason.
`modules/DevToolsStartup.sys.mjs` in `browser/omni.ja`:

```js
const kDebuggerPrefs = [
  "devtools.debugger.remote-enabled",
  "devtools.chrome.enabled",
];
...
handleDevToolsServerFlag(cmdLine, portOrPath) {
  if (!this._isRemoteDebuggingEnabled()) {
    return;                       // <-- silent early return
  }
```

`_isRemoteDebuggingEnabled()` requires **every** pref in `kDebuggerPrefs` to be
true. `librewolf.cfg` forces `devtools.debugger.remote-enabled` false with
plain `pref()`, which user.js cannot override (see the rule above). The flag is
accepted, nothing listens, and nothing in the log names the cause.

**Consequence for the "attach to a browser that has been open for days"
question:** on LibreWolf that cannot be done over RDP, because RDP cannot be
enabled at all without changing the application bundle. Marionette and BiDi are
startup-only, so they cannot reach it either. The remaining routes are the
extension layer, or the proxy (which needs only a pref change the user can make
in `about:config`, and no restart for new requests).

To get RDP, one of these is required, and each has a cost:

1. A different Gecko browser whose autoconfig does not force the pref
   (plain Firefox, Waterfox, Floorp). Cheapest if one is installed.
2. A copy of the bundle with `librewolf.cfg` patched. Works, but it is a second
   multi-hundred-megabyte application and it diverges from the real one.
3. Android Firefox over adb, where this pref is not forced.

## Readiness signalling

Gecko writes **`MarionetteActivePort`** into the profile directory when the
Marionette listener is up. That file, not a port probe, is the reliable
readiness signal; measured startup was **10–20 s** on a cold scratch profile.

Two things that look like failures and are not:

- `Crash Annotation GraphicsCriticalError: RenderCompositorSWGL failed mapping
  default framebuffer, no dt` appears on **successful** headless launches too.
  It is noise here, not a diagnosis.
- `.startup-incomplete` is present for the whole session and is removed on a
  clean shutdown. Its presence does not mean startup stalled.

## Three things that silently break a launch

**1. A pref written with the wrong type aborts the rest of `user.js`.**
Gecko stops applying the file and does not say which line did it. The symptom
is a browser that starts and never opens any transport. Observed with three
mistyped prefs at once:

```text
user_pref("browser.startup.homepage_override.mstone", false);   # string pref
user_pref("browser.region.network.url", false);                 # string pref
user_pref("browser.newtabpage.activity-stream.feeds.system.topstories", "");  # bool pref
```

Removing those three made every launch succeed. `lwdbg` has a regression test
over its own generated `user.js` for exactly this.

**2. Stray instances.** Ten abandoned headless instances on the machine made
every subsequent launch fail to open Marionette, with the same benign-looking
GFX message as the only output. Separate profiles and `--no-remote` are not
enough. Kill leftovers before measuring:

```bash
pgrep -f "lwdbg/profiles" | xargs kill 2>/dev/null
```

**3. Fingerprinting resistance.** See below — this one does not break the
launch, it breaks the *numbers*, which is worse.

## `privacy.resistFingerprinting` destroys the measurement

LibreWolf enables it for everyone. With it on, this build reported:

- `performance.timing.navigationStart` = 0
- `navigation.responseStart` negative
- `performance.getEntriesByType('resource')` **empty**

Every number the tool exists to collect was missing or nonsense, and nothing
raised an error. A measurement profile must set:

```text
user_pref("privacy.resistFingerprinting", false);
user_pref("privacy.resistFingerprinting.pbmode", false);
user_pref("privacy.reduceTimerPrecision", false);
```

This is safe **only** in a scratch profile. Never write it into a profile
anybody browses with: it removes a privacy protection they chose this browser
for. `lwdbg launch` refuses any profile directory it did not create.

## Navigation Timing is anchored in the wrong place

Gecko sets `performance.timeOrigin` at **document creation**, which on a
cross-document navigation happens *after* the response. Marks on the navigation
entry are relative to that origin, so they come out negative. Real values from
this machine:

```text
timeOrigin      1793695505591.42
fetchStart          -5295.14 ms      <- the fetch began 5.3 s before timeOrigin
requestStart         -311.30 ms
responseStart        -177.46 ms
loadEventEnd         +714.02 ms
```

Reading `loadEventEnd` directly gives **0.714 s** for a load that actually took
**6.0 s** — an 8.4x underestimate, silently. Every milestone must be shifted by
the earliest mark on the entry:

```js
const zero = Math.min(0, ...[n.startTime, n.fetchStart, n.redirectStart]
                             .filter(v => typeof v === 'number' && v !== 0));
const us = v => Math.round((v - zero) * 1000);
```

`lwdbg` does this and reports `diag_anchor_ms` with every result, so the zero
of a measurement is never implicit.

**Cross-check after fixing it**, same origin, same minute:

| | TTFB (median) | total/load (median) | wire bytes |
| --- | --- | --- | --- |
| Marionette | 0.248 s | 0.774 s | 1 956 136 |
| curl | 0.223 s | 0.545 s | 1 956 136 |

11% apart on TTFB and byte-identical on the wire, which is what agreement is
supposed to look like. Load exceeding curl's total is expected: the browser
also parses a 3 MB document and 16 embedded data URIs.

## Other `librewolf.cfg` settings worth knowing

```text
devtools.selfxss.count = 0              # pasting into the console is allowed
devtools.console.stdout.chrome = false  # chrome console does NOT reach stdout
```

The second means a script expecting chrome-side console output on stdout gets
nothing until the pref is flipped. `policies.json` includes
`LocalNetworkAccess` — check it first if loopback connections behave oddly.
There is no `DisableDeveloperTools` and no `BlockAboutConfig`.

## Native messaging — still unresolved, must be probed

Gecko loads native messaging manifests from a **vendor-specific** directory,
and the LibreWolf binary contains *both* `LibreWolf` and `Mozilla` vendor
strings, so the directory cannot be read off the bundle. Candidates:

```text
~/Library/Application Support/Mozilla/NativeMessagingHosts/
~/Library/Application Support/LibreWolf/NativeMessagingHosts/
```

Install into **both** and let the running instance decide. This is why a tool
that writes only the Mozilla path may silently fail to connect here.

## Extension signing — still open

`librewolf.cfg` has no `xpinstall.signatures.required` line, so the build
default governs. Three outcomes, in order of preference:

1. Not required → plain `.xpi` install works.
2. Required → AMO unlisted signing via `web-ext sign`.
3. Development → `about:debugging` → "Load Temporary Add-on". Unsigned, and
   **lost on restart**.
