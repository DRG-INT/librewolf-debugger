# Gecko debug transports

Three protocols. The choice between them is decided by the situation, not by
preference.

| Transport | Port | Flag | Reaches a running browser | Also spoken by |
| --- | --- | --- | --- | --- |
| Marionette | 2828 | `--marionette` | **no** | — |
| DevTools RDP | 6000 | `--start-debugger-server N` | **yes** | Android Firefox over adb |
| WebDriver BiDi | 9222 | `--remote-debugging-port N` | **no** | **Blink** |

**There is no CDP in Gecko.** It was removed. A tool that offers "Firefox over
CDP" is describing something that does not exist on a current build.

RDP is the only one that attaches to a process that is already running — and on
**LibreWolf it cannot be enabled at all** (`references/librewolf.md` has the
derivation from Gecko's own source). So for LibreWolf specifically: measurement
uses Marionette or BiDi with a fresh launch, and a days-old session is reachable
only through the extension layer or the proxy.

BiDi is the strategic one: it is the single transport in this tool that a Blink
browser also answers, so anything written against it is the cheapest to extend
to Chrome later.

## Wire format

Marionette and RDP share one framing: `<decimal-byte-length>:<JSON>`. The
length counts **bytes of UTF-8**, not characters — a title with an em dash in it
desynchronises a char-counting implementation for every packet after it.

**Marionette** carries a message id:

```text
out  [0, msgId, "WebDriver:Navigate", {"url": "https://example.com/"}]
in   [1, msgId, error|null, result]
```

On connect the server sends an **unprompted** greeting that is not a reply to
anything:

```json
{"applicationType": "gecko", "marionetteProtocol": 3}
```

A client that waits for its first reply before reading will consume the
greeting as that reply and be one packet out of step for the whole session.

**RDP** has no message id. Every packet carries `from`, the actor that sent it,
and a request is addressed with `to`. The discipline that makes correlation
work:

- a **reply** has `from` and no `type`;
- an **event** has `from` and a `type`.

Anything that is not the awaited reply must be **queued, not dropped**:
`evaluateJSAsync` answers with a bare `resultID` and the actual value arrives
later as an `evaluationResult` event. A client that discards unmatched packets
loses every script result.

**BiDi** is JSON over WebSocket: `{"id": n, "method": ..., "params": ...}` out,
and `{"id": n, "result": ...}`, `{"id": n, "error": ...}` or an unsolicited
`{"method": ..., "params": ...}` back. Values come back in a tagged
representation that has to be collapsed — and an unrepresentable value (a DOM
node, a function) must keep its tag rather than becoming `null`, because "I got
a node back" and "I got nothing back" are different bugs.

## Readiness

Do not probe the port immediately; Gecko binds late in startup. Measured
**10–20 s** on a cold scratch profile. The reliable signal for Marionette is
the **`MarionetteActivePort`** file appearing in the profile directory.

`marionette.port` in `user.js` is honoured (verified on 2900). Without it the
default 2828 is used, which collides between concurrent instances.

## Prefs

```text
devtools.debugger.remote-enabled   true    # RDP only; forced false on LibreWolf
devtools.chrome.enabled            true    # RDP only
devtools.debugger.force-local      true    # keep the listener on loopback
devtools.debugger.prompt-connection false  # scratch profiles ONLY
privacy.resistFingerprinting       false   # scratch profiles ONLY — see below
privacy.reduceTimerPrecision       false   # scratch profiles ONLY
```

`prompt-connection` defaults to true and raises a **modal** dialog on an
incoming connection, which freezes the browser to all further automation until
a human clicks it. Never write it false into a profile somebody browses with.

`resistFingerprinting` is not a privacy nicety here: with it on, resource
timing returns **empty** and `responseStart` returns **negative**. Every number
is missing or wrong, and nothing errors.

## Timing — read the anchor, not the mark

Gecko anchors `performance.timeOrigin` at document creation, which on a
cross-document navigation is *after* the response. Marks on the navigation
entry come out negative and reading them directly under-reports badly — a
measured 6.0 s load read as 0.714 s.

Shift every milestone to the entry's earliest mark:

```js
const zero = Math.min(0, ...[n.startTime, n.fetchStart, n.redirectStart]
                             .filter(v => typeof v === 'number' && v !== 0));
```

`lwdbg` reports `diag_anchor_ms` with every result so the zero is explicit.

`encodedBodySize` vs `decodedBodySize` on the entry is the browser's **own**
verdict on whether compression applied, reached independently of the proxy.
When the two disagree, one of them is instrumented wrong, and that must be
resolved before either number is used.

## Commands

```bash
# start a browser with transports, leave it running
lwdbg launch --browser /Applications/LibreWolf.app \
             --transport marionette --transport bidi

# see what a listening transport is
lwdbg attach --transport marionette --port 2828

# run code in it
lwdbg eval --transport marionette --port 2828 \
           --url https://example.com/ 'return document.title;'

# a full measurement, three runs, into the store
lwdbg measure https://example.com/ --via marionette \
              --browser /Applications/LibreWolf.app --runs 3

# the same target without a browser, as the regression anchor
lwdbg measure https://example.com/ --via curl --runs 3
```

Agreement between the last two within ~20% on TTFB is the check that the
browser path is instrumented correctly. Measured on this machine: 0.248 s vs
0.223 s, byte-identical on the wire.

## Before you measure

```bash
pgrep -f "lwdbg/profiles" | xargs kill 2>/dev/null
```

Ten abandoned instances made every launch fail to open Marionette, with a
benign-looking GFX warning as the only output. Separate profiles and
`--no-remote` do not prevent it.
