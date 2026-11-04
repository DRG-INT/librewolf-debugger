# Android Firefox over adb

```bash
lwdbg android devices        # attached, and whether each is usable yet
lwdbg android sockets        # Firefox's debugger socket, read off the device
lwdbg android attach --keep  # forward it, report what RDP says, leave it up
lwdbg android size
lwdbg android tap 540 1200
lwdbg android swipe 540 1600 540 600 400
lwdbg android text "hello%world"
lwdbg android key BACK
lwdbg android screenshot     # into the store, as a render row
lwdbg android proxy --host-port 8899
```

## Why this module is small

Android Firefox speaks the **same DevTools RDP** as desktop Gecko. Once
`adb forward` has turned the device's abstract socket into a local TCP port,
the Stage 2 RDP client connects to it unchanged:

```bash
lwdbg android attach --keep --port 6080
lwdbg attach --transport rdp --port 6080
lwdbg eval  --transport rdp --port 6080 'return document.title;'
```

Nothing here re-implements a protocol. That was the point of building the RDP
client first.

Note the asymmetry with the desktop: on **LibreWolf** the RDP listener cannot
be enabled at all, because an AutoConfig `pref()` forces its gate off. On
Android that pref is not forced, so RDP is the *working* transport there and
the unavailable one here.

## The socket name is read, never assumed

It is build- and package-dependent — release, beta, nightly and the Fenix forks
all differ. `lwdbg android sockets` greps `/proc/net/unix` on the device for a
line mentioning firefox/fenix/mozilla *and* debugger/devtools, and strips the
`@` that marks an abstract socket. A hard-coded name like
`firefox-debugger-socket` is the thing that breaks on somebody else's build.

## This is what multitouch actually means

`adb shell input` goes through the device's own input stack, so a tap is
indistinguishable from a finger as far as the browser is concerned.

RFB/VNC has **no multitouch in the protocol at all**, macOS has no per-user
attachable display server, and headless and screen-watching are mutually
exclusive. So "drive the screen as if the VNC display were multitouch" is not
something to bolt onto a VNC session — the device's own input path is the real
one, and it is simpler.

For watching rather than driving, `scrcpy` mirrors the screen with input
passthrough. It is optional and `doctor` reports it as such.

`lwdbg android text` **refuses** a string containing a quote, backtick or `$`.
`input text` runs through a shell on the device, so those characters are a
command injection into that shell. Send it in pieces, or set the value through
the browser's own API over RDP.

## Routing the device through the proxy

A phone on USB has no route to the laptop's loopback. `adb reverse` gives it
one:

```bash
lwdbg proxy start --only-host example.com &
lwdbg android proxy --host-port 8899
```

Then set the HTTP and HTTPS proxy in Firefox on the device to
`127.0.0.1:8899`; adb tunnels it back. The CA still has to be trusted on the
device, which needs both `security.enterprise_roots.enabled` in Firefox and an
Android-level certificate install — two steps, and the second one is a
Settings dialog nobody can automate for you.

## Prerequisites, in the order they usually go wrong

1. Developer options enabled on the device.
2. USB debugging enabled in Developer options.
3. The cable in **file transfer** mode, not charging-only.
4. **"Remote debugging via USB" enabled in Firefox's own settings** — a
   separate switch from Android's USB debugging, and the one most often
   missed.
5. The *Allow* prompt accepted on the device screen. Until then `adb devices`
   reports `unauthorized`, which `lwdbg android devices` translates rather
   than printing raw.

## adb itself

Not installed on this machine. The `android-commandlinetools` cask is present
but its payload is missing, so `platform-tools` has to come from somewhere.
`sudo` is interactive here, so this is the user's command to run:

```text
! brew install --cask android-platform-tools
```

`lwdbg android devices` prints that remedy, plus every path it searched, rather
than just failing. Searched: `PATH`,
`~/Library/Android/sdk/platform-tools/adb`, `$ANDROID_HOME/platform-tools/adb`,
`/opt/homebrew/share/android-commandlinetools/platform-tools/adb`,
`/usr/local/share/android-sdk/platform-tools/adb`.

## Android LibreWolf

There is no LibreWolf build for Android. When one appears, nothing here
changes: it is Gecko, so the socket enumeration and the RDP client apply
as-is, and the only open question is whether its own autoconfig forces the
same pref the desktop build does.
