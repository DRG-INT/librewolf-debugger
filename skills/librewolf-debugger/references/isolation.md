# Isolated and headless runs

Automated runs should not touch the profile the user browses with. Three
isolation levels, strongest first. macOS has **no jail** and no native
container; these are what actually exist.

| Level | Isolation | Cost |
| --- | --- | --- |
| **lima VM** | separate kernel, separate network stack | runs Linux Firefox, not the host's browser binary |
| **separate user** | UID separation, own home and profile | one-time `sudo`; same kernel |
| **`sandbox-exec`** | filesystem/network confinement, same UID | deprecated but functional; weakest |
| dedicated profile | none, just state separation | free; use at minimum |

**At minimum use a dedicated profile.** `lwdbg launch` creates one and writes
the prefs that automation needs, so the user's daily profile keeps its
`prompt-connection` dialog and its session.

For *network* measurement the lima VM's "wrong browser" caveat does not matter —
TTFB and throughput are properties of the path, not the build. It matters only
for render-level work (parity, screenshots), where the binary must be the real one.

## Dedicated profile (the default)

`lwdbg launch` writes a fresh profile with:

```text
user_pref("devtools.debugger.remote-enabled", true);
user_pref("devtools.chrome.enabled", true);
user_pref("devtools.debugger.prompt-connection", false);
user_pref("devtools.debugger.force-local", true);
user_pref("marionette.port", <port>);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
```

`prompt-connection` false is safe **only** because this profile is created for
automation and the user never browses with it. Never write it into a profile
they use.

## Separate user

Requires an interactive `sudo` — an agent cannot do it non-interactively. Ask
the user to run it:

```text
sudo sysadminctl -addUser debugagent -fullName "Debug Agent" -password -
sudo dscl . create /Users/debugagent IsHidden 1
sudo dseditgroup -o checkmember -m debugagent admin   # expect "no"
```

Then run under it with `sudo -u debugagent`. The browser bundle must be
readable by that user; a bundle on an external volume usually is
(`drwxr-xr-x`), but check rather than assume.

## sandbox-exec

```text
(version 1)
(allow default)
(deny file-write*)
(allow file-write* (subpath "<work dir>") (subpath "/private/var/folders") (subpath "/private/tmp"))
(allow network*)
```

Deprecated by Apple, still works. Gives no UID separation, so it protects the
filesystem and nothing else.

## Headless vs visible

`--headless` for every automated run. Headless and screen-watching are mutually
exclusive: there is no display to look at. To *see* a run, either run visibly on
the user's own display, or stay headless and take screenshots through the
transport. VNC is not an option on macOS — see `parity.md` for why.
