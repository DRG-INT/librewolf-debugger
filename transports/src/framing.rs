//! `<decimal-length>:<JSON>` packet framing.
//!
//! Marionette and the DevTools Remote Debugging Protocol both use this exact
//! wire format, so one codec serves both. The length counts **bytes** of the
//! UTF-8 JSON, not characters — a page title with an em dash in it breaks a
//! char-counting implementation.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// A JSON packet is refused above this size. The largest legitimate packet is
/// a full-page screenshot as base64, which for a 4K full-page capture is tens
/// of megabytes; 256 MiB is far above that and still bounds a malformed or
/// hostile length prefix.
pub const MAX_PACKET: usize = 256 * 1024 * 1024;

#[derive(Debug)]
pub struct Framed {
    stream: TcpStream,
    /// Bytes read from the socket but not yet consumed as a packet.
    buf: Vec<u8>,
}

impl Framed {
    pub fn connect(addr: &str, timeout: Duration) -> Result<Self> {
        let addrs: Vec<std::net::SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(&addr)
            .with_context(|| format!("resolving {addr}"))?
            .collect();
        let sa = addrs
            .first()
            .ok_or_else(|| anyhow!("{addr} resolved to nothing"))?;
        let stream = TcpStream::connect_timeout(sa, timeout)
            .with_context(|| format!("connecting to {addr}"))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream,
            buf: Vec::new(),
        })
    }

    pub fn set_timeout(&mut self, t: Duration) -> Result<()> {
        self.stream.set_read_timeout(Some(t))?;
        Ok(())
    }

    pub fn send(&mut self, v: &Value) -> Result<()> {
        let body = serde_json::to_vec(v)?;
        let mut out = format!("{}:", body.len()).into_bytes();
        out.extend_from_slice(&body);
        self.stream.write_all(&out).context("writing packet")?;
        self.stream.flush()?;
        Ok(())
    }

    /// Read exactly one packet, reading more from the socket as needed.
    pub fn recv(&mut self) -> Result<Value> {
        loop {
            if let Some(v) = self.take_packet()? {
                return Ok(v);
            }
            let mut chunk = [0u8; 16 * 1024];
            let n = self.stream.read(&mut chunk).context("reading packet")?;
            if n == 0 {
                bail!(
                    "the browser closed the connection with {} bytes buffered. A \
                     modal connection prompt, or a profile that was never started \
                     with the transport enabled, both look like this.",
                    self.buf.len()
                );
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Consume one complete packet from the buffer, if there is one.
    fn take_packet(&mut self) -> Result<Option<Value>> {
        let colon = match self.buf.iter().position(|&b| b == b':') {
            Some(i) => i,
            None => {
                // No separator yet. A long run of non-digits is not a slow
                // packet, it is the wrong protocol — say so instead of
                // buffering forever.
                if self.buf.len() > 32 && !self.buf[..32].iter().all(|b| b.is_ascii_digit()) {
                    bail!(
                        "no length prefix in the first bytes: {:?}. This port is not \
                         speaking Marionette or RDP.",
                        String::from_utf8_lossy(&self.buf[..32.min(self.buf.len())])
                    );
                }
                return Ok(None);
            }
        };
        // A colon is not proof of a length prefix: an HTTP response has one
        // in its first header line. Everything before it must be digits, or
        // this is simply not the protocol.
        let prefix = &self.buf[..colon];
        if prefix.is_empty() || !prefix.iter().all(u8::is_ascii_digit) {
            bail!(
                "no length prefix before the first colon: {:?}. This port is not \
                 speaking Marionette or RDP.",
                String::from_utf8_lossy(&self.buf[..colon.min(64)])
            );
        }
        let len: usize = std::str::from_utf8(prefix)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                anyhow!(
                    "length prefix {:?} is not a usable byte count",
                    String::from_utf8_lossy(prefix)
                )
            })?;
        if len > MAX_PACKET {
            bail!("packet claims {len} bytes, over the {MAX_PACKET} limit");
        }
        let start = colon + 1;
        if self.buf.len() < start + len {
            return Ok(None);
        }
        let body = self.buf[start..start + len].to_vec();
        self.buf.drain(..start + len);
        let v = serde_json::from_slice(&body).with_context(|| {
            format!(
                "parsing packet body {:?}",
                String::from_utf8_lossy(&body[..body.len().min(200)])
            )
        })?;
        Ok(Some(v))
    }
}

/// Frame a value the way the wire wants it. Exposed for tests and for anyone
/// who needs to hand-build a packet.
pub fn frame(v: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).expect("serializing a Value cannot fail");
    let mut out = format!("{}:", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn length_is_bytes_not_characters() {
        // "ö" is two bytes. A char-counting framer writes 1 and desynchronises
        // the stream for every packet after it.
        let v = json!(["ö"]);
        let f = frame(&v);
        let s = String::from_utf8_lossy(&f);
        let (len, body) = s.split_once(':').unwrap();
        assert_eq!(len.parse::<usize>().unwrap(), body.len());
        assert_eq!(len, "6"); // ["ö"] == 5 ASCII + 1 extra byte
    }

    #[test]
    fn frame_round_trips_through_take_packet() {
        let mut f = Framed {
            stream: dummy_stream(),
            buf: Vec::new(),
        };
        let a = json!([0, 1, "WebDriver:Navigate", {"url": "https://example.com/"}]);
        let b = json!([1, 1, null, {"value": null}]);
        f.buf.extend_from_slice(&frame(&a));
        f.buf.extend_from_slice(&frame(&b));
        assert_eq!(f.take_packet().unwrap().unwrap(), a);
        assert_eq!(f.take_packet().unwrap().unwrap(), b);
        assert!(f.take_packet().unwrap().is_none());
    }

    #[test]
    fn a_partial_packet_is_not_an_error() {
        let mut f = Framed {
            stream: dummy_stream(),
            buf: Vec::new(),
        };
        let full = frame(&json!({"from": "root"}));
        f.buf.extend_from_slice(&full[..full.len() - 3]);
        assert!(
            f.take_packet().unwrap().is_none(),
            "should wait for the rest"
        );
        f.buf.extend_from_slice(&full[full.len() - 3..]);
        assert!(f.take_packet().unwrap().is_some());
    }

    #[test]
    fn a_non_protocol_greeting_fails_fast() {
        let mut f = Framed {
            stream: dummy_stream(),
            buf: Vec::new(),
        };
        // An HTTP server on the port, for instance.
        f.buf
            .extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n");
        let e = f.take_packet().unwrap_err().to_string();
        assert!(e.contains("not speaking"), "got: {e}");
    }

    #[test]
    fn an_absurd_length_is_refused_before_allocating() {
        let mut f = Framed {
            stream: dummy_stream(),
            buf: Vec::new(),
        };
        f.buf.extend_from_slice(b"999999999999:");
        assert!(f
            .take_packet()
            .unwrap_err()
            .to_string()
            .contains("over the"));
    }

    /// A socket that is never read or written; `take_packet` only touches the
    /// buffer, so this keeps those tests free of a real listener.
    fn dummy_stream() -> TcpStream {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let s = TcpStream::connect(addr).unwrap();
        drop(l);
        s
    }
}
