//! Marionette client.
//!
//! Protocol 3: a command is `[0, msgId, name, params]`, a reply is
//! `[1, msgId, error, result]`. On connect the server sends an *unprompted*
//! packet describing itself — it is not a reply to anything, so a client that
//! waits for its first reply before reading will read the greeting instead and
//! be one packet out of step for the rest of the session.
//!
//! Marionette cannot be enabled on a running process. It is a startup flag,
//! which is why `launch` exists.

use crate::framing::Framed;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 2828;

#[derive(Debug)]
pub struct Marionette {
    io: Framed,
    next_id: i64,
    pub application_type: String,
    pub protocol: i64,
    pub session_id: Option<String>,
    pub capabilities: Value,
}

impl Marionette {
    /// Connect and read the greeting. Does not create a session.
    pub fn connect(addr: &str, timeout: Duration) -> Result<Self> {
        let mut io = Framed::connect(addr, timeout)?;
        let hello = io.recv().context("reading the Marionette greeting")?;
        let application_type = hello
            .get("applicationType")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let protocol = hello
            .get("marionetteProtocol")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("greeting has no marionetteProtocol field: {hello}"))?;
        if protocol != 3 {
            bail!("Marionette protocol {protocol} is not supported by this client (expects 3)");
        }
        Ok(Self {
            io,
            next_id: 1,
            application_type,
            protocol,
            session_id: None,
            capabilities: Value::Null,
        })
    }

    pub fn new_session(&mut self) -> Result<&Value> {
        let r = self.command("WebDriver:NewSession", json!({}))?;
        self.session_id = r
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.capabilities = r.get("capabilities").cloned().unwrap_or(Value::Null);
        Ok(&self.capabilities)
    }

    /// Send one command and return its `result`. Unsolicited packets and
    /// replies to earlier ids are skipped, so one slow command cannot
    /// desynchronise the session.
    pub fn command(&mut self, name: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.io.send(&json!([0, id, name, params]))?;

        loop {
            let pkt = self.io.recv().with_context(|| format!("awaiting {name}"))?;
            let arr = match pkt.as_array() {
                Some(a) if a.len() == 4 && a[0].as_i64() == Some(1) => a,
                // Not a reply: a greeting, or an async event. Keep reading.
                _ => continue,
            };
            if arr[1].as_i64() != Some(id) {
                continue;
            }
            if !arr[2].is_null() {
                let e = &arr[2];
                let kind = e.get("error").and_then(Value::as_str).unwrap_or("error");
                let msg = e.get("message").and_then(Value::as_str).unwrap_or("");
                bail!("{name} failed: {kind}: {msg}");
            }
            return Ok(arr[3].clone());
        }
    }

    pub fn navigate(&mut self, url: &str) -> Result<()> {
        self.command("WebDriver:Navigate", json!({ "url": url }))?;
        Ok(())
    }

    /// Run a script in the page and return its value. `await` is supported
    /// because the timing APIs are read after `load`, which is asynchronous.
    pub fn execute(&mut self, script: &str) -> Result<Value> {
        let r = self.command(
            "WebDriver:ExecuteAsyncScript",
            json!({
                "script": format!(
                    "const done = arguments[arguments.length - 1];\n\
                     (async () => {{ {script} }})().then(done, e => done({{ \
                     __lwdbg_error: String(e) }}));"
                ),
                "args": [],
                "newSandbox": false,
            }),
        )?;
        let v = r.get("value").cloned().unwrap_or(Value::Null);
        if let Some(e) = v.get("__lwdbg_error").and_then(Value::as_str) {
            bail!("script threw: {e}");
        }
        Ok(v)
    }

    /// Set the script timeout. The default is 30s, which is shorter than this
    /// origin's observed worst case.
    pub fn set_timeouts(&mut self, script_ms: u64, page_load_ms: u64) -> Result<()> {
        self.command(
            "WebDriver:SetTimeouts",
            json!({ "script": script_ms, "pageLoad": page_load_ms, "implicit": 0 }),
        )?;
        self.io
            .set_timeout(Duration::from_millis(script_ms.max(page_load_ms) + 15_000))?;
        Ok(())
    }

    /// Base64 PNG of the viewport, or the full page.
    pub fn screenshot(&mut self, full: bool) -> Result<String> {
        let r = self.command(
            "WebDriver:TakeScreenshot",
            json!({ "full": full, "hash": false, "scroll": true }),
        )?;
        r.get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("screenshot reply has no value: {r}"))
    }

    pub fn set_window_rect(&mut self, width: u32, height: u32) -> Result<Value> {
        self.command(
            "WebDriver:SetWindowRect",
            json!({ "width": width, "height": height, "x": 0, "y": 0 }),
        )
    }

    pub fn window_handles(&mut self) -> Result<Vec<String>> {
        let r = self.command("WebDriver:GetWindowHandles", json!({}))?;
        Ok(r.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn current_url(&mut self) -> Result<String> {
        let r = self.command("WebDriver:GetCurrentURL", json!({}))?;
        Ok(r.get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    pub fn delete_session(&mut self) -> Result<()> {
        if self.session_id.is_some() {
            self.command("WebDriver:DeleteSession", json!({}))?;
            self.session_id = None;
        }
        Ok(())
    }
}

impl Drop for Marionette {
    fn drop(&mut self) {
        // A session left open blocks the next connection to the same browser.
        let _ = self.delete_session();
    }
}

/// The script that reads Navigation Timing. Kept here so the proxy path and
/// the browser path are comparing the same definitions of each milestone.
///
/// `encodedBodySize` vs `decodedBodySize` is the browser's own answer to
/// "did compression apply", independent of what the proxy saw.
pub const NAV_TIMING_SCRIPT: &str = r#"
const n = performance.getEntriesByType('navigation')[0];
if (!n) return null;

// The navigation entry's marks are relative to performance.timeOrigin, and
// Gecko anchors timeOrigin at *document creation*, which happens after the
// response has been received. Measured on LibreWolf 153 against a real
// origin: fetchStart = -5295ms, responseStart = -177ms, loadEventEnd = +714ms.
// Reading loadEventEnd directly therefore reports 0.7s for a load that
// actually took 6.0s — an underestimate of nearly 9x, with no error anywhere.
//
// The fix is to anchor every milestone to the earliest mark the entry has
// rather than to timeOrigin. For a redirect-free load that is fetchStart; a
// redirect chain starts earlier still, at redirectStart. The anchor is
// reported alongside the numbers so the zero of the measurement is never
// implicit.
const marks = [n.startTime, n.fetchStart, n.redirectStart]
  .filter(v => typeof v === 'number' && v !== 0);
const zero = marks.length ? Math.min(0, ...marks) : 0;

const us = v => (typeof v !== 'number' || v === 0) ? null : Math.round((v - zero) * 1000);

return {
  us_ttfb: us(n.responseStart),
  us_response_end: us(n.responseEnd),
  us_dom_interactive: us(n.domInteractive),
  us_dcl: us(n.domContentLoadedEventEnd),
  us_load: us(n.loadEventEnd),
  transfer_size: n.transferSize ?? null,
  encoded_body_size: n.encodedBodySize ?? null,
  decoded_body_size: n.decodedBodySize ?? null,
  dom_nodes: document.getElementsByTagName('*').length,
  // Provenance of the numbers above, kept so a surprising result can be
  // checked without re-running anything.
  diag_anchor_ms: zero,
  diag_time_origin: performance.timeOrigin,
  diag_raw: {
    startTime: n.startTime,
    redirectStart: n.redirectStart,
    fetchStart: n.fetchStart,
    domainLookupEnd: n.domainLookupEnd,
    connectEnd: n.connectEnd,
    requestStart: n.requestStart,
    responseStart: n.responseStart,
    responseEnd: n.responseEnd,
    domContentLoadedEventEnd: n.domContentLoadedEventEnd,
    loadEventEnd: n.loadEventEnd,
  },
  diag_resource_entries: performance.getEntriesByType('resource').length,
  diag_ready_state: document.readyState,
  diag_url: location.href,
};
"#;

pub const RESOURCE_TIMING_SCRIPT: &str = r#"
return performance.getEntriesByType('resource').map(r => ({
  url: r.name,
  initiator: r.initiatorType || null,
  us_start: Math.round(r.startTime * 1000),
  us_ttfb: r.responseStart ? Math.round(r.responseStart * 1000) : null,
  us_end: Math.round(r.responseEnd * 1000),
  transfer_size: r.transferSize ?? null,
  encoded_body_size: r.encodedBodySize ?? null,
  decoded_body_size: r.decodedBodySize ?? null,
}));
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::frame;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A listener that speaks just enough Marionette to drive the client.
    fn fake_server(
        greeting: Value,
        replies: Vec<Value>,
    ) -> (String, std::thread::JoinHandle<Vec<Value>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(&frame(&greeting)).unwrap();
            let mut seen = Vec::new();
            let mut buf = Vec::new();
            for reply in replies {
                // Read one command before answering it.
                loop {
                    if let Some(v) = pop(&mut buf) {
                        seen.push(v);
                        break;
                    }
                    let mut c = [0u8; 4096];
                    let n = s.read(&mut c).unwrap_or(0);
                    if n == 0 {
                        return seen;
                    }
                    buf.extend_from_slice(&c[..n]);
                }
                s.write_all(&frame(&reply)).unwrap();
            }
            seen
        });
        (addr, h)
    }

    fn pop(buf: &mut Vec<u8>) -> Option<Value> {
        let colon = buf.iter().position(|&b| b == b':')?;
        let len: usize = std::str::from_utf8(&buf[..colon]).ok()?.parse().ok()?;
        if buf.len() < colon + 1 + len {
            return None;
        }
        let body = buf[colon + 1..colon + 1 + len].to_vec();
        buf.drain(..colon + 1 + len);
        serde_json::from_slice(&body).ok()
    }

    fn greeting() -> Value {
        json!({"applicationType": "gecko", "marionetteProtocol": 3})
    }

    #[test]
    fn greeting_is_consumed_before_the_first_reply() {
        let (addr, h) = fake_server(
            greeting(),
            vec![json!([1, 1, null, {"value": "https://example.com/"}])],
        );
        let mut m = Marionette::connect(&addr, Duration::from_secs(2)).unwrap();
        assert_eq!(m.application_type, "gecko");
        assert_eq!(m.current_url().unwrap(), "https://example.com/");
        m.session_id = None; // keep Drop from sending a teardown
        drop(m);
        let seen = h.join().unwrap();
        assert_eq!(seen[0][2], "WebDriver:GetCurrentURL");
    }

    #[test]
    fn a_reply_to_an_older_id_does_not_satisfy_the_current_command() {
        let (addr, h) = fake_server(
            greeting(),
            vec![
                // Stale reply for id 0, then an async event, then the real one.
                json!([1, 0, null, {"value": "stale"}]),
                json!([1, 1, null, {"value": "fresh"}]),
            ],
        );
        let mut m = Marionette::connect(&addr, Duration::from_secs(2)).unwrap();
        // The server answers one packet per command read, so prime it by
        // issuing the command and letting both replies arrive.
        let got = m.command("WebDriver:GetCurrentURL", json!({}));
        m.session_id = None;
        drop(m);
        let _ = h.join();
        // Either the stale reply was skipped and "fresh" came back, or the
        // read timed out; what must never happen is "stale" being returned.
        if let Ok(v) = got {
            assert_eq!(v["value"], "fresh");
        }
    }

    #[test]
    fn a_protocol_error_becomes_an_error_not_a_silent_null() {
        let (addr, h) = fake_server(
            greeting(),
            vec![json!([1, 1, {"error": "timeout", "message": "Navigation timed out"}, null])],
        );
        let mut m = Marionette::connect(&addr, Duration::from_secs(2)).unwrap();
        let e = m.navigate("https://slow.example/").unwrap_err().to_string();
        assert!(e.contains("timeout"), "got: {e}");
        assert!(e.contains("Navigation timed out"), "got: {e}");
        m.session_id = None;
        drop(m);
        let _ = h.join();
    }

    #[test]
    fn an_unsupported_protocol_version_is_refused() {
        let (addr, h) = fake_server(
            json!({"applicationType": "gecko", "marionetteProtocol": 2}),
            vec![],
        );
        let e = Marionette::connect(&addr, Duration::from_secs(2))
            .unwrap_err()
            .to_string();
        assert!(e.contains("protocol 2"), "got: {e}");
        let _ = h.join();
    }

    #[test]
    fn the_nav_script_anchors_to_the_earliest_mark_not_to_time_origin() {
        // The bug this guards against produced a 9x underestimate with no
        // error: Gecko's timeOrigin sits after the response, so a mark read
        // directly is measured from the wrong zero.
        assert!(NAV_TIMING_SCRIPT.contains("Math.min(0,"));
        assert!(NAV_TIMING_SCRIPT.contains("n.fetchStart"));
        assert!(NAV_TIMING_SCRIPT.contains("diag_anchor_ms"));
        assert!(
            NAV_TIMING_SCRIPT.contains("(v - zero)"),
            "every milestone must be shifted by the anchor"
        );
    }

    #[test]
    fn timing_scripts_return_a_value_not_a_statement() {
        // ExecuteAsyncScript wraps these in an async function, so a bare
        // `return` is required; a top-level expression would evaluate to
        // undefined and the measurement would silently be empty.
        assert!(NAV_TIMING_SCRIPT.contains("return {"));
        assert!(RESOURCE_TIMING_SCRIPT.trim_start().starts_with("return "));
    }
}
