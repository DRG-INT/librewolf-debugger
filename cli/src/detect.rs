//! Browser discovery.
//!
//! Resolving symlinks matters: on this machine `/Applications/LibreWolf.app`
//! is a symlink to an offload volume, and a detector that reports the link
//! path sends every later command to the wrong place.

use anyhow::Result;
use lwdbg_ssot::Engine;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
pub struct Found {
    pub name: String,
    pub engine: String,
    pub bundle_path: String,
    /// Present when `bundle_path` was reached through a symlink.
    pub resolved_path: Option<String>,
    pub binary: String,
    pub bundle_id: Option<String>,
    pub version: Option<String>,
    pub has_debug_transport: bool,
    pub supports_webextensions: bool,
    /// Why this browser is excluded from automation, when it is.
    pub excluded: Option<String>,
}

struct Known {
    bundle: &'static str,
    name: &'static str,
    binary: &'static str,
    engine: Engine,
}

/// Bundle names and their in-bundle executables. Gecko forks do not all name
/// the binary after the app.
const KNOWN: &[Known] = &[
    Known {
        bundle: "LibreWolf.app",
        name: "LibreWolf",
        binary: "librewolf",
        engine: Engine::Gecko,
    },
    Known {
        bundle: "Firefox.app",
        name: "Firefox",
        binary: "firefox",
        engine: Engine::Gecko,
    },
    Known {
        bundle: "Firefox Nightly.app",
        name: "Firefox Nightly",
        binary: "firefox",
        engine: Engine::Gecko,
    },
    Known {
        bundle: "Firefox Developer Edition.app",
        name: "Firefox Developer Edition",
        binary: "firefox",
        engine: Engine::Gecko,
    },
    Known {
        bundle: "Waterfox.app",
        name: "Waterfox",
        binary: "waterfox",
        engine: Engine::Gecko,
    },
    Known {
        bundle: "Floorp.app",
        name: "Floorp",
        binary: "floorp",
        engine: Engine::Gecko,
    },
    Known {
        bundle: "Zen Browser.app",
        name: "Zen",
        binary: "zen",
        engine: Engine::Gecko,
    },
    Known {
        bundle: "Mullvad Browser.app",
        name: "Mullvad Browser",
        binary: "mullvadbrowser",
        engine: Engine::Gecko,
    },
    Known {
        bundle: "Tor Browser.app",
        name: "Tor Browser",
        binary: "firefox",
        engine: Engine::Gecko,
    },
    Known {
        bundle: "IceCat.app",
        name: "GNU IceCat",
        binary: "icecat",
        engine: Engine::Gecko,
    },
    Known {
        bundle: "SeaMonkey.app",
        name: "SeaMonkey",
        binary: "seamonkey",
        engine: Engine::Gecko,
    },
    Known {
        bundle: "Pale Moon.app",
        name: "Pale Moon",
        binary: "palemoon",
        engine: Engine::Goanna,
    },
    Known {
        bundle: "Basilisk.app",
        name: "Basilisk",
        binary: "basilisk",
        engine: Engine::Goanna,
    },
    Known {
        bundle: "Google Chrome.app",
        name: "Google Chrome",
        binary: "Google Chrome",
        engine: Engine::Blink,
    },
    Known {
        bundle: "Chromium.app",
        name: "Chromium",
        binary: "Chromium",
        engine: Engine::Blink,
    },
    Known {
        bundle: "Microsoft Edge.app",
        name: "Microsoft Edge",
        binary: "Microsoft Edge",
        engine: Engine::Blink,
    },
    Known {
        bundle: "Brave Browser.app",
        name: "Brave",
        binary: "Brave Browser",
        engine: Engine::Blink,
    },
    Known {
        bundle: "Vivaldi.app",
        name: "Vivaldi",
        binary: "Vivaldi",
        engine: Engine::Blink,
    },
    Known {
        bundle: "Opera.app",
        name: "Opera",
        binary: "Opera",
        engine: Engine::Blink,
    },
    Known {
        bundle: "Arc.app",
        name: "Arc",
        binary: "Arc",
        engine: Engine::Blink,
    },
    Known {
        bundle: "Safari.app",
        name: "Safari",
        binary: "Safari",
        engine: Engine::WebKit,
    },
    Known {
        bundle: "Orion.app",
        name: "Orion",
        binary: "Orion",
        engine: Engine::WebKit,
    },
];

fn search_roots() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        v.push(PathBuf::from(home).join("Applications"));
    }
    v
}

fn plist_value(bundle: &Path, key: &str) -> Option<String> {
    let out = std::process::Command::new("/usr/libexec/PlistBuddy")
        .arg("-c")
        .arg(format!("Print :{key}"))
        .arg(bundle.join("Contents/Info.plist"))
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

pub fn detect() -> Result<Vec<Found>> {
    let mut found = Vec::new();
    for root in search_roots() {
        for k in KNOWN {
            let path = root.join(k.bundle);
            if !path.exists() {
                continue;
            }
            // `canonicalize` follows the symlink; report both so a surprising
            // install location is visible rather than silent.
            let resolved = std::fs::canonicalize(&path).ok();
            let real = resolved.clone().unwrap_or_else(|| path.clone());
            let binary = real.join("Contents/MacOS").join(k.binary);
            if !binary.exists() {
                continue;
            }

            let excluded = if k.name == "Tor Browser" {
                Some(
                    "excluded from proxy and transport defaults: a local MITM breaks its \
                     threat model (override with --force-tor)"
                        .to_string(),
                )
            } else if k.engine == Engine::Goanna {
                Some(
                    "proxy-only: Goanna has no usable debug transport and no WebExtensions \
                     support"
                        .to_string(),
                )
            } else {
                None
            };

            let shown_resolved = resolved
                .as_ref()
                .filter(|r| r.as_path() != path.as_path())
                .map(|r| r.display().to_string());

            found.push(Found {
                name: k.name.to_string(),
                engine: k.engine.as_str().to_string(),
                bundle_path: path.display().to_string(),
                resolved_path: shown_resolved,
                binary: binary.display().to_string(),
                bundle_id: plist_value(&real, "CFBundleIdentifier"),
                version: plist_value(&real, "CFBundleShortVersionString"),
                has_debug_transport: k.engine.has_debug_transport(),
                supports_webextensions: k.engine.supports_webextensions(),
                excluded,
            });
        }
    }
    Ok(found)
}
