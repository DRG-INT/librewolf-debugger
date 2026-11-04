//! DevTools Remote Debugging Protocol client — the "Firefox debugging server".
//!
//! Same `<len>:<JSON>` framing as Marionette, different semantics: there is no
//! message id. Every packet carries `from`, the actor that sent it, and a
//! request is addressed with `to`. Correlation is therefore *by actor*, and
//! the discipline that makes it work is:
//!
//! * a **reply** has `from` and no `type`;
//! * an **event** has `from` and a `type`.
//!
//! Anything that is not the reply being awaited is queued, never dropped —
//! `evaluateJSAsync` returns a bare `resultID` first and the actual value
//! arrives later as an `evaluationResult` event, so a client that discards
//! unmatched packets loses every script result.
//!
//! Two things about this transport are worth knowing before using it:
//!
//! 1. Unlike Marionette, it **can** be enabled at startup on a profile that
//!    is also used interactively, and `about:debugging` uses it. That is what
//!    makes it the route to an already-running browser and to Android.
//! 2. `devtools.debugger.prompt-connection` defaults to true and raises a
//!    **modal** dialog on connect, which freezes the browser to further
//!    automation until a human clicks it.
//!
//! Actor names and reply shapes are version-dependent. Nothing here asserts a
//! shape it did not read: `describe()` returns what the browser actually said.

use crate::framing::Framed;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::time::Duration;

/// `--start-debugger-server` with no argument uses 6000.
pub const DEFAULT_PORT: u16 = 6000;

#[derive(Debug)]
pub struct Rdp {
    io: Framed,
    /// Packets read while awaiting some other reply.
    pending: VecDeque<Value>,
    pub root: Value,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Tab {
    pub actor: String,
    pub url: String,
    pub title: String,
    pub selected: bool,
    pub browser_id: Option<i64>,
}

impl Rdp {
    /// Connect and consume the root actor's greeting.
    pub fn connect(addr: &str, timeout: Duration) -> Result<Self> {
        let mut io = Framed::connect(addr, timeout)?;
        let root = io.recv().context(
            "reading the root actor greeting. If this times out, a connection \
             prompt is probably waiting to be clicked in the browser \
             (devtools.debugger.prompt-connection)",
        )?;
        if root.get("from").and_then(Value::as_str) != Some("root") {
            bail!("first packet is not from the root actor: {root}");
        }
        Ok(Self {
            io,
            pending: VecDeque::new(),
            root,
        })
    }

    pub fn set_timeout(&mut self, t: Duration) -> Result<()> {
        self.io.set_timeout(t)
    }

    /// Send a request to an actor and return its reply.
    pub fn request(&mut self, to: &str, kind: &str, extra: Value) -> Result<Value> {
        let mut msg = json!({ "to": to, "type": kind });
        if let (Some(obj), Some(add)) = (msg.as_object_mut(), extra.as_object()) {
            for (k, v) in add {
                obj.insert(k.clone(), v.clone());
            }
        }
        self.io.send(&msg)?;

        // A reply already queued from an earlier wait still counts.
        if let Some(i) = self.pending.iter().position(|p| is_reply_from(p, to)) {
            let v = self.pending.remove(i).unwrap();
            return check_error(v, kind);
        }

        loop {
            let pkt = self
                .io
                .recv()
                .with_context(|| format!("awaiting the reply to {kind}"))?;
            if is_reply_from(&pkt, to) {
                return check_error(pkt, kind);
            }
            self.pending.push_back(pkt);
            if self.pending.len() > 10_000 {
                bail!("10000 unmatched packets while awaiting {kind}; giving up");
            }
        }
    }

    /// Wait for an event of a given `type`, optionally matching a predicate.
    pub fn wait_event(&mut self, kind: &str, matches: impl Fn(&Value) -> bool) -> Result<Value> {
        if let Some(i) = self
            .pending
            .iter()
            .position(|p| p.get("type").and_then(Value::as_str) == Some(kind) && matches(p))
        {
            return Ok(self.pending.remove(i).unwrap());
        }
        loop {
            let pkt = self
                .io
                .recv()
                .with_context(|| format!("awaiting a {kind} event"))?;
            if pkt.get("type").and_then(Value::as_str) == Some(kind) && matches(&pkt) {
                return Ok(pkt);
            }
            self.pending.push_back(pkt);
        }
    }

    /// Events read but not consumed. Useful for logging everything the
    /// transport volunteered.
    pub fn drain_pending(&mut self) -> Vec<Value> {
        self.pending.drain(..).collect()
    }

    pub fn get_root(&mut self) -> Result<Value> {
        self.request("root", "getRoot", json!({}))
    }

    /// Open tabs. The reply shape changed across Firefox versions, so both
    /// the descriptor form and the older inline form are accepted.
    pub fn list_tabs(&mut self) -> Result<Vec<Tab>> {
        let r = self.request("root", "listTabs", json!({}))?;
        let arr = r
            .get("tabs")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("listTabs reply has no tabs array: {r}"))?;
        Ok(arr
            .iter()
            .map(|t| Tab {
                actor: t
                    .get("actor")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                url: t
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                title: t
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                selected: t.get("selected").and_then(Value::as_bool).unwrap_or(false),
                browser_id: t.get("browserId").and_then(Value::as_i64),
            })
            .collect())
    }

    /// Everything the browser will say about itself. This is the packet
    /// `about:debugging` renders, and on Android it is how the device and
    /// build are identified.
    pub fn describe(&mut self) -> Result<Value> {
        let root = self.get_root()?;
        let device = root.get("deviceActor").and_then(Value::as_str);
        let mut out = json!({ "root": root.clone(), "greeting": self.root.clone() });
        if let Some(d) = device {
            match self.request(d, "getDescription", json!({})) {
                Ok(v) => {
                    out["description"] = v.get("value").cloned().unwrap_or(v);
                }
                Err(e) => out["description_error"] = json!(e.to_string()),
            }
        } else {
            out["description_error"] = json!("getRoot reply has no deviceActor on this build");
        }
        Ok(out)
    }

    /// The target form for a tab: the actor ids its console, thread and
    /// resources live on.
    pub fn tab_target(&mut self, tab_actor: &str) -> Result<Value> {
        let r = self.request(tab_actor, "getTarget", json!({}))?;
        Ok(r.get("frame").cloned().unwrap_or(r))
    }

    /// The active tab and its console actor, in one step.
    ///
    /// Every caller that wants to run script needs exactly this pair, and
    /// getting it takes three round trips with two shapes that differ between
    /// builds. Doing it here means one implementation to keep correct instead
    /// of one per call site.
    pub fn active_console(&mut self) -> Result<(Tab, String)> {
        let tabs = self.list_tabs()?;
        let tab = tabs
            .iter()
            .find(|t| t.selected)
            .or_else(|| tabs.first())
            .ok_or_else(|| anyhow!("RDP reported no tabs"))?
            .clone();
        let target = self.tab_target(&tab.actor)?;
        let console = target
            .get("consoleActor")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!(
                    "the tab target has no consoleActor on this build; got keys {:?}",
                    target
                        .as_object()
                        .map(|o| o.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                )
            })?
            .to_string();
        Ok((tab, console))
    }

    /// Evaluate JavaScript in a tab's content window.
    ///
    /// The reply is only a receipt; the value arrives later as an
    /// `evaluationResult` event carrying the same `resultID`.
    pub fn eval(&mut self, console_actor: &str, text: &str) -> Result<Value> {
        let ack = self.request(console_actor, "evaluateJSAsync", json!({ "text": text }))?;
        let want = ack
            .get("resultID")
            .and_then(Value::as_str)
            .map(str::to_string);
        let ev = match want {
            Some(id) => self.wait_event("evaluationResult", move |p| {
                p.get("resultID").and_then(Value::as_str) == Some(id.as_str())
            })?,
            // Older builds answer synchronously.
            None => return Ok(ack),
        };
        if let Some(exc) = ev.get("exceptionMessage").and_then(Value::as_str) {
            if !exc.is_empty() {
                bail!("evaluation threw: {exc}");
            }
        }
        Ok(ev.get("result").cloned().unwrap_or(ev))
    }
}

fn is_reply_from(p: &Value, to: &str) -> bool {
    p.get("from").and_then(Value::as_str) == Some(to) && p.get("type").is_none()
}

fn check_error(v: Value, kind: &str) -> Result<Value> {
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        let msg = v.get("message").and_then(Value::as_str).unwrap_or("");
        bail!("{kind} failed: {e}: {msg}");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::frame;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Server that writes a greeting, then answers each request it reads with
    /// the next scripted packet batch.
    fn fake(
        greeting: Value,
        batches: Vec<Vec<Value>>,
    ) -> (String, std::thread::JoinHandle<Vec<Value>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(&frame(&greeting)).unwrap();
            let mut seen = Vec::new();
            let mut buf = Vec::new();
            for batch in batches {
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
                for p in batch {
                    s.write_all(&frame(&p)).unwrap();
                }
            }
            // Keep the socket open so the client is not told "closed" early.
            std::thread::sleep(Duration::from_millis(50));
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

    fn hello() -> Value {
        json!({"from": "root", "applicationType": "browser", "traits": {}})
    }

    #[test]
    fn greeting_must_come_from_root() {
        let (addr, h) = fake(json!({"from": "server0.conn0"}), vec![]);
        let e = Rdp::connect(&addr, Duration::from_secs(2))
            .unwrap_err()
            .to_string();
        assert!(e.contains("not from the root actor"), "got: {e}");
        let _ = h.join();
    }

    #[test]
    fn an_event_before_the_reply_is_queued_not_dropped() {
        let (addr, h) = fake(
            hello(),
            vec![vec![
                // An event arrives first, from the very actor being asked.
                json!({"from": "root", "type": "tabListChanged"}),
                json!({"from": "root", "tabs": [
                    {"actor": "a1", "url": "https://example.com/", "title": "Example",
                     "selected": true, "browserId": 7}
                ]}),
            ]],
        );
        let mut r = Rdp::connect(&addr, Duration::from_secs(2)).unwrap();
        let tabs = r.list_tabs().unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].actor, "a1");
        assert_eq!(tabs[0].browser_id, Some(7));
        // The event survived, so a log of everything the transport said is
        // still complete.
        let left = r.drain_pending();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0]["type"], "tabListChanged");
        let _ = h.join();
    }

    #[test]
    fn eval_waits_for_the_matching_result_id() {
        let (addr, h) = fake(
            hello(),
            vec![vec![
                json!({"from": "console1", "resultID": "r2"}),
                // A result for somebody else's evaluation, then ours.
                json!({"from": "console1", "type": "evaluationResult",
                       "resultID": "r1", "result": "wrong"}),
                json!({"from": "console1", "type": "evaluationResult",
                       "resultID": "r2", "result": 42}),
            ]],
        );
        let mut r = Rdp::connect(&addr, Duration::from_secs(2)).unwrap();
        assert_eq!(r.eval("console1", "40 + 2").unwrap(), json!(42));
        let _ = h.join();
    }

    #[test]
    fn an_exception_in_the_page_is_an_error() {
        let (addr, h) = fake(
            hello(),
            vec![vec![
                json!({"from": "console1", "resultID": "r1"}),
                json!({"from": "console1", "type": "evaluationResult",
                       "resultID": "r1", "exceptionMessage": "x is not defined"}),
            ]],
        );
        let mut r = Rdp::connect(&addr, Duration::from_secs(2)).unwrap();
        let e = r.eval("console1", "x").unwrap_err().to_string();
        assert!(e.contains("x is not defined"), "got: {e}");
        let _ = h.join();
    }

    #[test]
    fn an_actor_error_reply_is_surfaced() {
        let (addr, h) = fake(
            hello(),
            vec![vec![
                json!({"from": "root", "error": "unrecognizedPacketType",
                             "message": "actor root does not recognize listTabs"}),
            ]],
        );
        let mut r = Rdp::connect(&addr, Duration::from_secs(2)).unwrap();
        let e = r.list_tabs().unwrap_err().to_string();
        assert!(e.contains("unrecognizedPacketType"), "got: {e}");
        let _ = h.join();
    }

    #[test]
    fn describe_reports_the_absence_of_a_device_actor_instead_of_inventing_one() {
        let (addr, h) = fake(
            hello(),
            vec![vec![json!({"from": "root", "preferenceActor": "pref1"})]],
        );
        let mut r = Rdp::connect(&addr, Duration::from_secs(2)).unwrap();
        let d = r.describe().unwrap();
        assert!(d["description_error"]
            .as_str()
            .unwrap()
            .contains("no deviceActor"));
        let _ = h.join();
    }
}
