//! Gecko debug transports.
//!
//! Three protocols, one crate, because the choice between them is a property
//! of the situation and not of the task:
//!
//! | Transport | Reaches a running browser | Enabled by | Also on |
//! |---|---|---|---|
//! | Marionette | no — startup flag only | `--marionette` | — |
//! | DevTools RDP | **yes** | `--start-debugger-server`, or already on | Android over adb |
//! | WebDriver BiDi | no — startup flag only | `--remote-debugging-port` | **Blink** |
//!
//! The consequence worth remembering: a browser that has been open for days
//! can only be reached over RDP (or the extension layer). Marionette and BiDi
//! require a restart, and a restart loses the session that made the browser
//! interesting.
//!
//! RDP is also the Android transport. The client in `rdp` is used unchanged
//! there — `adb forward` turns the device's abstract socket into a local port,
//! and from this crate's point of view nothing else differs.

pub mod bidi;
pub mod framing;
pub mod launch;
pub mod marionette;
pub mod mozlog;
pub mod rdp;
pub mod ws;

use serde::Serialize;

/// Which transport a command should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Marionette,
    Rdp,
    Bidi,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Marionette => "marionette",
            Transport::Rdp => "rdp",
            Transport::Bidi => "bidi",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Transport::Marionette => marionette::DEFAULT_PORT,
            Transport::Rdp => rdp::DEFAULT_PORT,
            Transport::Bidi => bidi::DEFAULT_PORT,
        }
    }

    /// Whether this transport can attach to a process that is already
    /// running. This single fact decides most transport choices.
    pub fn attaches_to_running(self) -> bool {
        matches!(self, Transport::Rdp)
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "marionette" => Transport::Marionette,
            "rdp" | "devtools" => Transport::Rdp,
            "bidi" | "webdriver-bidi" => Transport::Bidi,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rdp_attaches_to_a_running_browser() {
        assert!(Transport::Rdp.attaches_to_running());
        assert!(!Transport::Marionette.attaches_to_running());
        assert!(!Transport::Bidi.attaches_to_running());
    }

    #[test]
    fn default_ports_are_the_documented_ones() {
        assert_eq!(Transport::Marionette.default_port(), 2828);
        assert_eq!(Transport::Rdp.default_port(), 6000);
        assert_eq!(Transport::Bidi.default_port(), 9222);
    }

    #[test]
    fn transport_names_round_trip() {
        for t in [Transport::Marionette, Transport::Rdp, Transport::Bidi] {
            assert_eq!(Transport::parse(t.as_str()), Some(t));
        }
        assert_eq!(Transport::parse("devtools"), Some(Transport::Rdp));
        // CDP is not a Gecko transport and must not silently map to one.
        assert_eq!(Transport::parse("cdp"), None);
    }
}
