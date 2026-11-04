//! Environment check. First call in any unfamiliar environment: it reports
//! what is missing instead of letting a later command fail obscurely.

use anyhow::Result;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    /// What to do about it, when it is actionable.
    pub remedy: Option<String>,
}

fn which(bin: &str) -> Option<String> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn tool(name: &str, remedy: &str, required: bool) -> Check {
    match which(name) {
        Some(p) => Check {
            name: name.into(),
            ok: true,
            detail: p,
            remedy: None,
        },
        None => Check {
            name: name.into(),
            ok: !required,
            detail: if required {
                "missing (required)".into()
            } else {
                "missing (optional)".into()
            },
            remedy: Some(remedy.into()),
        },
    }
}

pub fn run() -> Result<Vec<Check>> {
    let mut v = vec![
        tool(
            "curl",
            "part of macOS; a missing curl means a broken PATH",
            true,
        ),
        tool(
            "certutil",
            "brew install nss — needed to install the proxy CA into a Gecko profile's cert9.db",
            true,
        ),
        tool(
            "adb",
            "brew install --cask android-platform-tools — only needed for Android work",
            false,
        ),
        tool(
            "scrcpy",
            "brew install scrcpy — only needed for Android screen mirroring",
            false,
        ),
        tool(
            "sqlite3",
            "part of macOS; useful for ad-hoc SSoT queries",
            false,
        ),
    ];

    // Native messaging manifest directories. Which one LibreWolf reads cannot
    // be read off the bundle, so both are reported and the extension layer
    // installs into both.
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home);
        for (label, rel) in [
            (
                "native-messaging:Mozilla",
                "Library/Application Support/Mozilla/NativeMessagingHosts",
            ),
            (
                "native-messaging:LibreWolf",
                "Library/Application Support/LibreWolf/NativeMessagingHosts",
            ),
        ] {
            let p = home.join(rel);
            v.push(Check {
                name: label.into(),
                ok: true,
                detail: if p.is_dir() {
                    format!("{} (exists)", p.display())
                } else {
                    format!("{} (absent)", p.display())
                },
                remedy: None,
            });
        }
    }

    // Does sudo need a password? Agents cannot answer an interactive prompt,
    // so say so up front rather than hanging later.
    let sudo_nopass = std::process::Command::new("sudo")
        .args(["-n", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    v.push(Check {
        name: "sudo".into(),
        ok: true,
        detail: if sudo_nopass {
            "passwordless".into()
        } else {
            "requires an interactive password".into()
        },
        remedy: if sudo_nopass {
            None
        } else {
            Some(
                "commands needing sudo must be run by the user; in Claude Code prefix them \
                 with `! `"
                    .into(),
            )
        },
    });

    // The live bridge. Three separate things can be missing, and the
    // difference matters: the host binary, the manifest, and the extension
    // actually being loaded in a running browser.
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home);
        let manifest_found: Vec<String> = ["Mozilla", "LibreWolf", "Firefox"]
            .iter()
            .map(|vendor| {
                home.join("Library/Application Support")
                    .join(vendor)
                    .join("NativeMessagingHosts/net.lizicska.lwdbg.json")
            })
            .filter(|p| p.is_file())
            .map(|p| p.display().to_string())
            .collect();
        v.push(Check {
            name: "live:manifest".into(),
            ok: !manifest_found.is_empty(),
            detail: if manifest_found.is_empty() {
                "no native messaging manifest installed".into()
            } else {
                manifest_found.join(", ")
            },
            remedy: manifest_found
                .is_empty()
                .then(|| "lwdbg live install".to_string()),
        });

        let sock = home.join(".local/share/lwdbg/store/live.sock");
        let connected = sock.exists();
        v.push(Check {
            name: "live:bridge".into(),
            ok: connected,
            detail: if connected {
                format!("{} (the extension is loaded and connected)", sock.display())
            } else {
                "no socket — the extension is not loaded in any running browser".into()
            },
            remedy: (!connected).then(|| {
                "load the extension: about:debugging#/runtime/this-firefox -> Load \
                 Temporary Add-on -> extension/manifest.json. The host is started by \
                 the browser, not by lwdbg."
                    .to_string()
            }),
        });
    }

    // The GPU path lives in a separate binary, so what matters here is
    // whether *that* binary is installed — not a feature of this one. `lwdbg`
    // links no wgpu at all by design: doing so is what made its release build
    // unfinishable on this 8 GiB machine.
    //
    // `ok` stays true either way. The CPU implementation is the reference the
    // shader is tested against, so a machine without the helper is not a
    // broken environment, and `lwdbg capabilities --json` is what a script
    // should read.
    let helper = which("lwdbg-gpu").or_else(|| {
        // Used to locate this program's own data files, not to make an
        // authorization decision. Anyone who can replace the executable
        // already has everything this would protect.
        // nosemgrep: rust.lang.security.current-exe.current-exe
        std::env::current_exe()
            .ok()
            .map(|e| e.with_file_name("lwdbg-gpu"))
            .filter(|p| p.is_file())
            .map(|p| p.display().to_string())
    });
    v.push(Check {
        name: "gpu:helper".into(),
        ok: true,
        detail: match &helper {
            Some(p) => format!("{p} — `lwdbg parity --backend gpu` delegates to it"),
            None => "not installed; parity runs on the CPU reference path".into(),
        },
        remedy: helper.is_none().then(|| {
            "./configure --with-gpu && make — a second binary, so that lwdbg \
             itself never links wgpu; see references/building.md"
                .to_string()
        }),
    });

    // Leftover browser instances. Ten of them made every launch fail to open
    // Marionette on this machine, with a benign-looking GFX warning as the
    // only symptom, so this is worth a line of its own.
    let strays = std::process::Command::new("sh")
        .arg("-c")
        .arg("pgrep -f 'lwdbg/profiles' 2>/dev/null | wc -l")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<usize>()
                .ok()
        })
        .unwrap_or(0);
    v.push(Check {
        name: "stray-browsers".into(),
        ok: strays == 0,
        detail: format!("{strays} abandoned lwdbg browser process(es)"),
        remedy: (strays > 0).then(|| {
            "pgrep -f 'lwdbg/profiles' | xargs kill — leftovers make later launches \
             fail to open any transport"
                .to_string()
        }),
    });

    Ok(v)
}
