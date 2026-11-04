# The extension layer — reaching a session that must not be restarted

```bash
lwdbg live install        # host binary + native messaging manifests
lwdbg live commands       # what the connected extension can do
lwdbg live tabs
lwdbg live windows
lwdbg live timing --tab 7
lwdbg live screenshot
lwdbg live call navigate --params '{"url":"https://example.com/"}' --allow-write
```

## Why it exists

Marionette and WebDriver BiDi are **startup-only** flags. On LibreWolf the
DevTools RDP listener cannot be enabled at all. So a browser that has been
open for three days is unreachable by every transport, and restarting it loses
the session that made it interesting.

An extension runs *inside* that process. No restart, no lost session.

## Shape

```text
  browser ──spawns──> lwdbg-host ──unix socket──> lwdbg live ...
             stdio                ~/.local/share/lwdbg/store/live.sock
```

The browser starts the host, not the CLI. That inversion is the thing to
remember when `lwdbg live tabs` says it cannot reach the bridge: it means the
extension is not loaded, not that a daemon failed to start.

The host owns three jobs:

- **framing** — native messaging is a u32 **little-endian** length followed by
  UTF-8 JSON. Getting the endianness backwards makes the browser wait for a
  16-million-byte message forever, which is why there is a test asserting that
  `{}` frames as `02 00 00 00`.
- **correlation** — several `lwdbg` invocations can be in flight, so each gets
  an id and replies route back by it, not by arrival order.
- **policy** — see below.

Every diagnostic in the host goes to **stderr**. A stray `println!` on stdout
would corrupt the frame stream, and the symptom would be the extension going
quiet with no error.

## The write gate

The extension declares, per command, whether it changes state. The host
refuses a mutating command unless the caller passed `allow_write`:

| read | mutating |
| --- | --- |
| `ping`, `tabs`, `windows`, `timing`, `screenshot` | `eval`, `navigate`, `newTab`, `closeTab`, `reload` |

`eval` counts as mutating even though it is usually used to read, because a
page cannot be asked to promise it will not act.

An **unknown** command is treated as mutating. Defaulting to allowed would mean
a newer extension could bypass the gate, and refusing is the safe answer when
the catalogue has not arrived yet.

The refusal happens **before** anything is forwarded to the browser. There is a
test for exactly that — a refusal that still reached the browser would be
decoration, not a control.

This is a mechanism, not a substitute for judgement. `lwdbg live` acts on the
user's real session, with their logins and cookies. No payments, transfers,
destructive or irreversible actions, no exfiltration of personal data, and
explicit approval before each mutating action — every time, because approval
for one action is not approval for the next.

## The extension does not touch the network

Its only channel is the native messaging port, which the browser itself
spawns. There is no `fetch`, no socket, no host permission used for anything
but `executeScript`. An extension that could reach the network would be a far
larger grant than this job needs.

There is also no "evaluate the incoming message as code" path. Commands are
dispatched by name from a fixed table, and an unknown name is refused with the
list of known ones.

## The LibreWolf shim: both vendor directories

Gecko loads native messaging manifests from a **vendor-specific** directory,
and the LibreWolf binary contains *both* the `LibreWolf` and the `Mozilla`
vendor string. Which one it reads cannot be determined from the bundle, so
`lwdbg live install` writes to every candidate and lets the running instance
decide:

```text
~/Library/Application Support/Mozilla/NativeMessagingHosts/net.lizicska.lwdbg.json
~/Library/Application Support/LibreWolf/NativeMessagingHosts/net.lizicska.lwdbg.json
~/Library/Application Support/Firefox/NativeMessagingHosts/...   (if present)
```

A tool that writes only the Mozilla path fails silently here, which is one of
the reasons the upstream Firefox CLIs did not work on this machine.

## Loading it

```text
about:debugging#/runtime/this-firefox  →  Load Temporary Add-on...
→  <repo>/extension/manifest.json
```

Unsigned, and **lost on restart**. That is acceptable for the use case: the
session being reached is already running, and reloading the extension does not
restart the browser.

A permanent install needs one of:

1. `xpinstall.signatures.required` false — `librewolf.cfg` has no line for it,
   so the build default governs and this is still unverified. Note that if the
   build sets it with plain `pref()` the way it does the devtools gate, a user
   pref will not override it.
2. AMO unlisted signing via `web-ext sign`.

MV2 on purpose: Gecko 153 still supports it, `persistent: true` keeps the
native port alive, and an MV3 service worker would be torn down between
commands.

## Known limit

`browser.tabs.captureVisibleTab` captures the **visible** area only. There is
no full-page capture through this path; parity work on a long page uses
Marionette or BiDi with `--full-page`.
