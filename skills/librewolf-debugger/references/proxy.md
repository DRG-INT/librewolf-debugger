# The proxy layer

The proxy is the backbone because it is the only layer that reaches every
engine — including Goanna (Pale Moon, Basilisk), where no extension can go.

It does three jobs at once:

1. **Cache** — content-addressed on disk, so a slow origin is paid for once.
2. **Capture** — every request/response pair lands in the SSoT under a trace ID.
3. **Archive** — the cache *is* a site snapshot; `lwdbg proxy export` writes it out.

## Why cache, not just capture

A slow origin makes measurement expensive and non-reproducible. Caching turns
the second and later loads into a local-latency baseline, which is what you
need to separate *origin* cost from *parse and render* cost. Keep the
distinction explicit in any report: a cached load does not tell you what a
real visitor experiences.

## Modes

| Mode | How clients reach it | When |
| --- | --- | --- |
| **explicit** | browser proxy setting → `127.0.0.1:<port>` | default; works everywhere, no privileges |
| **transparent** | packet redirection (`pf` on macOS) | only when a client cannot be configured; **needs sudo** |

Prefer explicit. Transparent mode exists for clients with no proxy setting, and
it costs a root-level firewall rule — do not reach for it by default.

## HTTPS and the CA

Capturing HTTPS means terminating TLS, which means a locally-trusted CA. This
is the sharpest edge in the whole tool.

**Gecko forks do not use the system keychain.** Each *profile* carries its own
NSS trust store in `cert9.db`. A CA in the macOS keychain is invisible to them
unless `security.enterprise_roots.enabled` is true.

Two ways in, both supported:

```bash
lwdbg proxy ca install --profile <path>   # writes cert9.db with certutil
lwdbg proxy ca install --enterprise-roots # flips the pref, uses system store
```

`certutil` is the more surgical option: it touches exactly one profile and
leaves the system store alone. It is present on this machine at
`/opt/homebrew/bin/certutil`. Prefer it.

The `--enterprise-roots` route is one pref per profile but makes the browser
trust **every** CA in the system store, which is a wider change than the task
needs. Use it only where `certutil` cannot run.

**Blink and WebKit** do use the system keychain, so for those the CA is
installed once, system-wide — and that is a genuinely broad grant. Say so to
the user before doing it.

## Safety rules

- A trusted-CA proxy sees **everything** the browser sends, including
  credentials, session cookies and PII. This is not a side effect; it is how it
  works.
- Scope every capture: `--only-host <host>` restricts what is recorded.
  Everything else is tunneled without interception (no TLS termination).
- Do not leave a capture-all proxy running after a task. `lwdbg proxy stop`
  also reports what was captured so nothing is forgotten in the store.
- **Never proxy Tor Browser.** It defeats its threat model. `lwdbg` refuses
  unless `--force-tor` is passed.
- Uninstall the CA when the work is done: `lwdbg proxy ca uninstall`.

## Android

Two routes, both in `android.md`:

- `adb reverse` so the device reaches the host's loopback proxy over USB.
- Wi-Fi proxy settings, when USB is not available.

The CA must also be trusted on the device. Firefox for Android keeps its own
trust store, so `security.enterprise_roots.enabled` is the practical route
there; Android's own user-CA store is not trusted by apps by default since
Android 7.
