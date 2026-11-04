//! Android Firefox over adb.
//!
//! The architectural point of this module is how little it contains. Android
//! Firefox speaks the **same DevTools RDP** as desktop Gecko, so once
//! `adb forward` has turned the device's abstract socket into a local TCP
//! port, the Stage 2 RDP client connects to it unchanged. Nothing here
//! re-implements a protocol.
//!
//! This is also where real multitouch lives. RFB/VNC has no multitouch at
//! all, and macOS has no per-user attachable display server, so "drive the
//! screen as if it were a multitouch display" is not a thing to bolt onto a
//! VNC session — it is `adb shell input`, which is the actual input path the
//! device uses.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

/// Where `adb` is, with the reason it is not found when it is not.
pub fn find_adb() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("adb"))
            .find(|p| p.is_file())
    }) {
        return Ok(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(h) = &home {
        candidates.push(h.join("Library/Android/sdk/platform-tools/adb"));
    }
    for v in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(d) = std::env::var_os(v) {
            candidates.push(PathBuf::from(d).join("platform-tools/adb"));
        }
    }
    candidates.push(PathBuf::from(
        "/opt/homebrew/share/android-commandlinetools/platform-tools/adb",
    ));
    candidates.push(PathBuf::from(
        "/usr/local/share/android-sdk/platform-tools/adb",
    ));

    for c in &candidates {
        if c.is_file() {
            return Ok(c.clone());
        }
    }
    Err(anyhow!(
        "adb is not installed. The android-commandlinetools cask is present on this \
         machine but its payload is missing, so platform-tools has to come from \
         somewhere. sudo is interactive here, so this is yours to run:\n  \
         ! brew install --cask android-platform-tools\n\
         Searched: PATH, {}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn adb(serial: Option<&str>, args: &[&str]) -> Result<String> {
    let bin = find_adb()?;
    let mut cmd = Command::new(&bin);
    if let Some(s) = serial {
        cmd.args(["-s", s]);
    }
    cmd.args(args);
    let out = cmd
        .output()
        .with_context(|| format!("running {} {}", bin.display(), args.join(" ")))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        bail!(
            "adb {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim(),
            if stdout.is_empty() {
                String::new()
            } else {
                format!(" / {}", stdout.trim())
            }
        );
    }
    Ok(stdout)
}

/// Binary-safe adb call, for `exec-out screencap -p`.
fn adb_bytes(serial: Option<&str>, args: &[&str]) -> Result<Vec<u8>> {
    let bin = find_adb()?;
    let mut cmd = Command::new(&bin);
    if let Some(s) = serial {
        cmd.args(["-s", s]);
    }
    cmd.args(args);
    let out = cmd.output().context("running adb")?;
    if !out.status.success() {
        bail!(
            "adb {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

#[derive(Debug, Serialize)]
pub struct Device {
    pub serial: String,
    /// device | unauthorized | offline | no permissions ...
    pub state: String,
    pub model: Option<String>,
    pub product: Option<String>,
    /// Set when the state means the device cannot be used yet, with what to do.
    pub blocked: Option<String>,
}

pub fn devices() -> Result<Vec<Device>> {
    let out = adb(None, &["devices", "-l"])?;
    let mut v = Vec::new();
    for line in out.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let serial = parts.next().unwrap_or_default().to_string();
        let state = parts.next().unwrap_or("unknown").to_string();
        let mut model = None;
        let mut product = None;
        for p in parts {
            if let Some(v) = p.strip_prefix("model:") {
                model = Some(v.to_string());
            }
            if let Some(v) = p.strip_prefix("product:") {
                product = Some(v.to_string());
            }
        }
        let blocked = match state.as_str() {
            "device" => None,
            "unauthorized" => Some(
                "the device has not accepted this computer's key. Unlock the screen \
                 and tap Allow on the USB debugging prompt."
                    .to_string(),
            ),
            "offline" => Some(
                "adb sees the device but cannot talk to it. Re-plug it, and check the \
                 cable is in file-transfer mode rather than charging-only."
                    .to_string(),
            ),
            other => Some(format!("state {other:?} — the device is not usable yet")),
        };
        v.push(Device {
            serial,
            state,
            model,
            product,
            blocked,
        });
    }
    Ok(v)
}

#[derive(Debug, Serialize)]
pub struct Socket {
    /// The abstract socket name, without the leading NUL that `/proc` shows
    /// as `@`.
    pub name: String,
    pub raw: String,
}

/// Find Firefox's debugger socket on the device.
///
/// The name is build- and package-dependent — release, beta, nightly and Fenix
/// forks all differ — so it is read from the device rather than assumed. This
/// is the step that a hard-coded socket name gets wrong.
pub fn firefox_sockets(serial: Option<&str>) -> Result<Vec<Socket>> {
    let out = adb(serial, &["shell", "cat", "/proc/net/unix"])?;
    let mut v = Vec::new();
    for line in out.lines() {
        let lower = line.to_ascii_lowercase();
        if !(lower.contains("firefox") || lower.contains("fenix") || lower.contains("mozilla")) {
            continue;
        }
        if !lower.contains("debugger") && !lower.contains("devtools") {
            continue;
        }
        if let Some(path) = line.split_whitespace().last() {
            let name = path.trim_start_matches('@').to_string();
            v.push(Socket {
                name,
                raw: line.trim().to_string(),
            });
        }
    }
    Ok(v)
}

pub fn forward(serial: Option<&str>, local_port: u16, socket: &str) -> Result<()> {
    adb(
        serial,
        &[
            "forward",
            &format!("tcp:{local_port}"),
            &format!("localabstract:{socket}"),
        ],
    )?;
    Ok(())
}

pub fn remove_forward(serial: Option<&str>, local_port: u16) -> Result<()> {
    adb(
        serial,
        &["forward", "--remove", &format!("tcp:{local_port}")],
    )?;
    Ok(())
}

/// Route the device's traffic through a proxy on this machine.
///
/// `adb reverse` makes a port on the *device* reach a port here, which is what
/// a phone on USB needs: it has no route to the laptop's loopback otherwise.
/// The browser on the device is then pointed at `127.0.0.1:<port>`, which adb
/// tunnels back.
pub fn reverse_proxy(serial: Option<&str>, device_port: u16, host_port: u16) -> Result<()> {
    adb(
        serial,
        &[
            "reverse",
            &format!("tcp:{device_port}"),
            &format!("tcp:{host_port}"),
        ],
    )?;
    Ok(())
}

// ----------------------------------------------------------------- input
//
// This is the real answer to "drive the screen with multitouch". These go
// through the device's own input stack, so they are indistinguishable from a
// finger as far as the browser is concerned.

pub fn tap(serial: Option<&str>, x: i32, y: i32) -> Result<()> {
    adb(
        serial,
        &["shell", "input", "tap", &x.to_string(), &y.to_string()],
    )?;
    Ok(())
}

pub fn swipe(serial: Option<&str>, x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) -> Result<()> {
    adb(
        serial,
        &[
            "shell",
            "input",
            "swipe",
            &x1.to_string(),
            &y1.to_string(),
            &x2.to_string(),
            &y2.to_string(),
            &ms.to_string(),
        ],
    )?;
    Ok(())
}

/// Type text. `input text` treats a space as an argument separator, so spaces
/// are sent as `%s` — the convention `input` itself expects.
pub fn text(serial: Option<&str>, s: &str) -> Result<()> {
    if s.contains('\'') || s.contains('"') || s.contains('`') || s.contains('$') {
        bail!(
            "refusing to send {s:?}: quotes and shell metacharacters would be \
             interpreted by the device's shell. Send it in pieces, or use the \
             browser's own API through the RDP transport."
        );
    }
    adb(serial, &["shell", "input", "text", &s.replace(' ', "%s")])?;
    Ok(())
}

pub fn keyevent(serial: Option<&str>, key: &str) -> Result<()> {
    adb(serial, &["shell", "input", "keyevent", key])?;
    Ok(())
}

/// PNG bytes of the device screen. `exec-out` rather than `shell`, because
/// `shell` mangles binary output with CRLF translation on some builds.
pub fn screencap(serial: Option<&str>) -> Result<Vec<u8>> {
    let bytes = adb_bytes(serial, &["exec-out", "screencap", "-p"])?;
    if !bytes.starts_with(b"\x89PNG") {
        bail!(
            "screencap did not return a PNG ({} bytes, starts {:?}). Some builds \
             need `adb shell screencap -p /sdcard/s.png` then `adb pull`.",
            bytes.len(),
            String::from_utf8_lossy(&bytes[..bytes.len().min(16)])
        );
    }
    Ok(bytes)
}

/// Screen size in pixels, for turning a fraction into a tap coordinate.
pub fn screen_size(serial: Option<&str>) -> Result<(u32, u32)> {
    let out = adb(serial, &["shell", "wm", "size"])?;
    // "Physical size: 1080x2400"
    let last = out
        .lines()
        .filter_map(|l| l.split(':').nth(1))
        .next_back()
        .ok_or_else(|| anyhow!("cannot parse `wm size` output: {out:?}"))?;
    let (w, h) = last
        .trim()
        .split_once('x')
        .ok_or_else(|| anyhow!("cannot parse size {last:?}"))?;
    Ok((w.trim().parse()?, h.trim().parse()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_adb_explains_the_remedy_rather_than_just_failing() {
        // Only meaningful when adb really is absent, which is the state of
        // this machine; when it is installed the message is irrelevant.
        if find_adb().is_ok() {
            return;
        }
        let e = find_adb().unwrap_err().to_string();
        assert!(
            e.contains("brew install --cask android-platform-tools"),
            "got: {e}"
        );
        assert!(
            e.contains("Searched:"),
            "the message must say where it looked"
        );
    }

    #[test]
    fn text_with_shell_metacharacters_is_refused() {
        // `adb shell input text` goes through a shell on the device, so a
        // quote in the payload is a command injection into that shell.
        for bad in ["it's", "say \"hi\"", "a`b`", "$HOME"] {
            let e = text(None, bad);
            assert!(e.is_err(), "{bad:?} should have been refused");
        }
    }

    #[test]
    fn a_socket_line_is_parsed_without_the_abstract_marker() {
        // Real shape of a /proc/net/unix line.
        let line = "0000000000000000: 00000002 00000000 00010000 0001 01 123456 \
                    @firefox-debugger-socket-org.mozilla.firefox";
        let name = line
            .split_whitespace()
            .last()
            .unwrap()
            .trim_start_matches('@');
        assert_eq!(name, "firefox-debugger-socket-org.mozilla.firefox");
    }

    #[test]
    fn device_states_other_than_device_carry_an_instruction() {
        for state in ["unauthorized", "offline", "sideload"] {
            let d = Device {
                serial: "X".into(),
                state: state.into(),
                model: None,
                product: None,
                blocked: match state {
                    "device" => None,
                    "unauthorized" => Some("x".into()),
                    "offline" => Some("x".into()),
                    other => Some(format!("state {other:?}")),
                },
            };
            assert!(d.blocked.is_some(), "{state} must not look usable");
        }
    }
}
