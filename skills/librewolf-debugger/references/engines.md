# Engine, transport and extension matrix

Checked 2026-11. Share figures are Statcounter-derived secondary sources and
approximate; the *capability* columns are what matter here.

## Engines

| Engine | Share | Browsers | Notes |
| --- | --- | --- | --- |
| **Blink** | ~78–79% | Chrome, Edge, Brave, Opera, Vivaldi, Arc, Chromium | Forked from WebKit's WebCore on 2013-04-03. Uses V8, not JavaScriptCore. Genealogically WebKit-descended, **operationally a separate engine**. |
| **WebKit** | ~15–18% | Safari, GNOME Web, Orion, Konqueror | iOS was WebKit-only by policy; the EU DMA has since opened that. |
| **Gecko** | ~3% | Firefox + all forks below | |
| **Goanna** | — | Pale Moon, Basilisk | Gecko fork, diverged around Gecko 52. Far enough to count as its own engine. |
| **Servo** | — | — | Independent, Rust. Its components (Stylo, WebRender) were absorbed into Gecko. |
| **Ladybird** | — | Ladybird | Independent, written from scratch (LibWeb). Alpha target reported as 2026; not confirmed from an official source. |

**"Quantum" is not an engine.** It is the name of a Gecko improvement
programme that landed Servo-derived components. Firefox's engine is Gecko. If a
document or a person says "Quantum engine", they mean Gecko.

## Transports — three families, not one

| Engine | Transports | CDP? |
| --- | --- | --- |
| Gecko | Marionette (2828), DevTools RDP (6000), WebDriver BiDi (9222) | **No.** Removed. 0 hits for `remote/cdp` in `omni.ja`. |
| Blink | CDP, increasingly WebDriver BiDi | Yes, native |
| WebKit | WebKit Remote Inspector, `safaridriver` | No |
| Goanna | none usable | No |

**WebDriver BiDi is the only transport Gecko and Blink share.** When support
widens beyond Gecko, that is the cheapest path — one adapter, two engines.

## Extensions — not unifiable into one artifact

| Engine | Extension model | Native messaging |
| --- | --- | --- |
| Gecko | WebExtensions (MV2 still supported in 153) | Yes |
| Blink | WebExtensions MV3, different API surface | Yes, different manifest location |
| WebKit / Safari | Safari Web Extension, needs Xcode packaging and a host app wrapper | Different mechanism |
| **Goanna** | **None.** Own legacy add-on system. | No |

This is the reason the proxy is the backbone and not the extension: a single
extension cannot reach every engine, and **Pale Moon/Basilisk cannot be reached
by any extension at all**. The proxy reaches all of them.

## Gecko fork matrix

`lwdbg detect` fills this in at runtime. Static expectations:

| Fork | Engine | Transports | Extension | Notes |
| --- | --- | --- | --- | --- |
| Firefox | Gecko | all three | yes | reference implementation |
| LibreWolf | Gecko | all three | yes | hardened prefs via `librewolf.cfg`; see `librewolf.md` |
| Waterfox | Gecko | all three | yes | |
| Floorp | Gecko | all three | yes | Chromium-based before early 2022, Gecko since |
| Zen | Gecko | all three | yes | |
| Mullvad Browser | Gecko | all three | yes | anti-fingerprinting; expect pref lockdown |
| Tor Browser | Gecko | all three | yes | **do not proxy**: it has its own transport; proxying breaks its threat model |
| GNU IceCat | Gecko | all three | yes | oldest fork still maintained |
| SeaMonkey | Gecko | partial | yes | suite, older Gecko base |
| **Pale Moon** | Goanna | none | **none** | proxy-only |
| **Basilisk** | Goanna | none | **none** | proxy-only |

**Tor Browser is deliberately excluded from proxy and transport defaults.**
Routing it through a local MITM proxy defeats its purpose. `lwdbg` refuses it
unless `--force-tor` is passed, and `detect` flags it.
