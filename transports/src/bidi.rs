//! WebDriver BiDi client.
//!
//! BiDi matters here for one reason that has nothing to do with Gecko: it is
//! the **only** transport in this tool that a Blink browser also speaks. Every
//! command written against this module is a command that will later work
//! against Chrome without being rewritten, which is why the engine-opening
//! work is cheapest at this layer.
//!
//! Shape of the protocol: JSON over WebSocket, `{"id": n, "method": ..,
//! "params": ..}` out, and either `{"id": n, "result": ..}`,
//! `{"id": n, "error": ..}` or an unsolicited `{"method": .., "params": ..}`
//! event back. Correlation is by `id`, so unlike RDP there is no actor
//! bookkeeping — events are simply queued.
//!
//! Gecko enables the BiDi listener with `--remote-debugging-port`. The
//! endpoint path is build-dependent, so `connect` tries the documented
//! `/session` and falls back to the bare root rather than asserting one.

use crate::ws::WebSocket;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 9222;

#[derive(Debug)]
pub struct Bidi {
    ws: WebSocket,
    next_id: i64,
    events: VecDeque<Value>,
    pub session_id: Option<String>,
    pub endpoint: String,
}

impl Bidi {
    /// Connect to the BiDi WebSocket. Tries each candidate path and reports
    /// every failure if none works — a wrong path and a closed port produce
    /// very different errors, and both are worth seeing.
    pub fn connect(host: &str, port: u16, timeout: Duration) -> Result<Self> {
        let mut errors = Vec::new();
        for path in ["/session", "/"] {
            let url = format!("ws://{host}:{port}{path}");
            match WebSocket::connect(&url, timeout) {
                Ok(ws) => {
                    return Ok(Self {
                        ws,
                        next_id: 1,
                        events: VecDeque::new(),
                        session_id: None,
                        endpoint: url,
                    })
                }
                Err(e) => errors.push(format!("{url}: {e}")),
            }
        }
        Err(anyhow!(
            "no BiDi endpoint answered on {host}:{port}\n  {}",
            errors.join("\n  ")
        ))
    }

    pub fn set_timeout(&mut self, t: Duration) -> Result<()> {
        self.ws.set_timeout(t)
    }

    pub fn send(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.ws.send_text(&serde_json::to_string(&json!({
            "id": id, "method": method, "params": params
        }))?)?;

        loop {
            let txt = self
                .ws
                .recv_text()
                .with_context(|| format!("awaiting the response to {method}"))?;
            let v: Value = serde_json::from_str(&txt)
                .with_context(|| format!("parsing a BiDi message: {txt:.300}"))?;
            match v.get("id").and_then(Value::as_i64) {
                Some(got) if got == id => {
                    if let Some(e) = v.get("error") {
                        let msg = v.get("message").and_then(Value::as_str).unwrap_or_default();
                        bail!("{method} failed: {}: {msg}", e.as_str().unwrap_or("error"));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                // Another client's response, or an event. Keep it.
                _ => {
                    self.events.push_back(v);
                    if self.events.len() > 100_000 {
                        bail!("100000 queued events while awaiting {method}");
                    }
                }
            }
        }
    }

    pub fn new_session(&mut self) -> Result<Value> {
        let r = self.send(
            "session.new",
            json!({ "capabilities": { "alwaysMatch": {} } }),
        )?;
        self.session_id = r
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(r)
    }

    pub fn subscribe(&mut self, events: &[&str]) -> Result<Value> {
        self.send("session.subscribe", json!({ "events": events }))
    }

    /// Top-level browsing contexts. The first one is the usable target.
    pub fn contexts(&mut self) -> Result<Vec<String>> {
        let r = self.send("browsingContext.getTree", json!({ "maxDepth": 0 }))?;
        Ok(r.get("contexts")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.get("context").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Navigate and wait. `wait` is one of `none`, `interactive`, `complete`;
    /// `complete` is the `load` event, which is what a page-load measurement
    /// means by "done".
    pub fn navigate(&mut self, context: &str, url: &str, wait: &str) -> Result<Value> {
        self.send(
            "browsingContext.navigate",
            json!({ "context": context, "url": url, "wait": wait }),
        )
    }

    /// Evaluate and return the deserialized value.
    pub fn evaluate(&mut self, context: &str, expr: &str) -> Result<Value> {
        let r = self.send(
            "script.evaluate",
            json!({
                "expression": expr,
                "target": { "context": context },
                "awaitPromise": true,
                "resultOwnership": "none",
                "serializationOptions": { "maxObjectDepth": 10, "maxDomDepth": 0 },
            }),
        )?;
        if r.get("type").and_then(Value::as_str) == Some("exception") {
            let text = r
                .pointer("/exceptionDetails/text")
                .and_then(Value::as_str)
                .unwrap_or("unknown exception");
            bail!("evaluation threw: {text}");
        }
        Ok(deserialize(r.get("result").unwrap_or(&Value::Null)))
    }

    /// Base64 PNG.
    pub fn screenshot(&mut self, context: &str, full: bool) -> Result<String> {
        let r = self.send(
            "browsingContext.captureScreenshot",
            json!({
                "context": context,
                "origin": if full { "document" } else { "viewport" },
            }),
        )?;
        r.get("data")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("captureScreenshot reply has no data: {r}"))
    }

    pub fn drain_events(&mut self) -> Vec<Value> {
        self.events.drain(..).collect()
    }

    pub fn end_session(&mut self) -> Result<()> {
        if self.session_id.is_some() {
            let _ = self.send("session.end", json!({}));
            self.session_id = None;
        }
        Ok(())
    }
}

/// BiDi returns values in its own tagged representation. Collapse it to plain
/// JSON, because every consumer here wants the value, not the type tag.
///
/// Unrepresentable things (a DOM node, a function) keep their tag rather than
/// becoming `null`: "I got a node back" and "I got nothing back" are different
/// bugs and must not look alike.
pub fn deserialize(v: &Value) -> Value {
    let Some(t) = v.get("type").and_then(Value::as_str) else {
        return v.clone();
    };
    match t {
        "undefined" | "null" => Value::Null,
        "string" | "boolean" => v.get("value").cloned().unwrap_or(Value::Null),
        "number" => match v.get("value") {
            // Infinity, -Infinity and NaN arrive as strings.
            Some(Value::String(s)) => json!(s),
            Some(n) => n.clone(),
            None => Value::Null,
        },
        "bigint" => v.get("value").cloned().unwrap_or(Value::Null),
        "array" | "set" => Value::Array(
            v.get("value")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(deserialize).collect())
                .unwrap_or_default(),
        ),
        "object" | "map" => {
            // Pairs: [[key, value], ...] where key is a string or a tagged value.
            let mut m = serde_json::Map::new();
            if let Some(pairs) = v.get("value").and_then(Value::as_array) {
                for p in pairs {
                    let Some(kv) = p.as_array() else { continue };
                    if kv.len() != 2 {
                        continue;
                    }
                    let k = match &kv[0] {
                        Value::String(s) => s.clone(),
                        other => deserialize(other)
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| other.to_string()),
                    };
                    m.insert(k, deserialize(&kv[1]));
                }
            }
            Value::Object(m)
        }
        _ => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_collapse_to_plain_json() {
        assert_eq!(
            deserialize(&json!({"type": "string", "value": "x"})),
            json!("x")
        );
        assert_eq!(
            deserialize(&json!({"type": "number", "value": 42})),
            json!(42)
        );
        assert_eq!(
            deserialize(&json!({"type": "boolean", "value": true})),
            json!(true)
        );
        assert_eq!(deserialize(&json!({"type": "undefined"})), Value::Null);
        assert_eq!(deserialize(&json!({"type": "null"})), Value::Null);
    }

    #[test]
    fn special_numbers_survive_as_strings_instead_of_becoming_null() {
        assert_eq!(
            deserialize(&json!({"type": "number", "value": "NaN"})),
            json!("NaN")
        );
        assert_eq!(
            deserialize(&json!({"type": "number", "value": "Infinity"})),
            json!("Infinity")
        );
    }

    #[test]
    fn the_nav_timing_object_round_trips_through_deserialization() {
        // This is the actual shape script.evaluate returns for the timing
        // script, and the reason deserialize exists at all.
        let raw = json!({
            "type": "object",
            "value": [
                ["us_ttfb", {"type": "number", "value": 221000}],
                ["us_load", {"type": "number", "value": 503000}],
                ["encoded_body_size", {"type": "number", "value": 1956136}],
                ["decoded_body_size", {"type": "number", "value": 3076779}],
                ["blocked", {"type": "null"}]
            ]
        });
        let v = deserialize(&raw);
        assert_eq!(v["us_ttfb"], 221000);
        assert_eq!(v["decoded_body_size"], 3076779);
        assert!(v["blocked"].is_null());
        // Compression is decidable from the browser's own numbers.
        assert!(
            v["encoded_body_size"].as_i64().unwrap() < v["decoded_body_size"].as_i64().unwrap()
        );
    }

    #[test]
    fn nested_arrays_of_objects_work_for_resource_timing() {
        let raw = json!({
            "type": "array",
            "value": [
                {"type": "object", "value": [["url", {"type": "string", "value": "https://a/"}]]},
                {"type": "object", "value": [["url", {"type": "string", "value": "https://b/"}]]}
            ]
        });
        let v = deserialize(&raw);
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[1]["url"], "https://b/");
    }

    #[test]
    fn an_unrepresentable_value_keeps_its_tag_rather_than_pretending_to_be_null() {
        let node = json!({"type": "node", "sharedId": "abc"});
        assert_eq!(deserialize(&node), node);
    }

    #[test]
    fn an_untagged_value_passes_through_unchanged() {
        assert_eq!(deserialize(&json!({"a": 1})), json!({"a": 1}));
    }

    #[test]
    fn connect_failure_names_every_endpoint_it_tried() {
        // Port 1 is unbound; both candidate paths must appear in the error so
        // a wrong path is distinguishable from a dead port.
        let e = Bidi::connect("127.0.0.1", 1, Duration::from_millis(120))
            .unwrap_err()
            .to_string();
        assert!(e.contains("/session"), "got: {e}");
        assert!(e.contains("ws://127.0.0.1:1/"), "got: {e}");
    }
}
