//! A minimal WebSocket client: exactly what WebDriver BiDi needs, nothing
//! more.
//!
//! BiDi is the only one of the three Gecko transports that is not raw TCP, so
//! something has to speak RFC 6455. This is ~200 lines of it rather than a
//! dependency, because the subset required is small and fully determined:
//! loopback, no TLS, text frames, client-side masking.
//!
//! What is implemented, and what is deliberately not:
//!
//! * masking of every client frame — **mandatory** per RFC 6455 §5.3; an
//!   unmasked client frame is a protocol error the server must close on;
//! * `Sec-WebSocket-Accept` is verified, so a plain HTTP server answering 101
//!   by accident is caught rather than producing garbled frames later;
//! * server→client fragmentation is reassembled;
//! * ping is answered with pong, so the connection survives an idle page load;
//! * no `permessage-deflate`: it is never negotiated, so it can never arrive.
//! * no TLS: BiDi here is always `ws://127.0.0.1`. A `wss://` URL is refused
//!   rather than silently downgraded.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// A single BiDi message above this is not a message, it is a mistake.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct WebSocket {
    stream: TcpStream,
    buf: Vec<u8>,
    mask_seed: u64,
}

impl WebSocket {
    /// Connect to `ws://host:port/path`.
    pub fn connect(url: &str, timeout: Duration) -> Result<Self> {
        if url.starts_with("wss://") {
            bail!("wss:// is not supported; BiDi on loopback is ws://");
        }
        let rest = url
            .strip_prefix("ws://")
            .ok_or_else(|| anyhow!("{url} is not a ws:// url"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };

        let sa = std::net::ToSocketAddrs::to_socket_addrs(&authority)
            .with_context(|| format!("resolving {authority}"))?
            .next()
            .ok_or_else(|| anyhow!("{authority} resolved to nothing"))?;
        let mut stream = TcpStream::connect_timeout(&sa, timeout)
            .with_context(|| format!("connecting to {authority}"))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true).ok();

        let seed = seed();
        let key = b64(&seed.to_be_bytes().repeat(2)[..16]);
        let req = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {authority}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(req.as_bytes())?;
        stream.flush()?;

        // Read headers, keeping any frame bytes that arrived with them.
        let mut buf = Vec::new();
        let head_end = loop {
            if let Some(i) = find(&buf, b"\r\n\r\n") {
                break i + 4;
            }
            let mut c = [0u8; 2048];
            let n = stream
                .read(&mut c)
                .context("reading the upgrade response")?;
            if n == 0 {
                bail!("the server closed the connection during the WebSocket upgrade");
            }
            buf.extend_from_slice(&c[..n]);
            if buf.len() > 64 * 1024 {
                bail!("upgrade response headers exceeded 64 KiB");
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        buf.drain(..head_end);

        let status = head.lines().next().unwrap_or_default();
        if !status.contains(" 101") {
            bail!(
                "expected HTTP 101 from the WebSocket upgrade, got {:?}. A BiDi \
                 port that answers 200 is usually the wrong port.",
                status
            );
        }
        let expect = b64(&sha1(format!("{key}{GUID}").as_bytes()));
        let got = header(&head, "sec-websocket-accept")
            .ok_or_else(|| anyhow!("101 response without Sec-WebSocket-Accept"))?;
        if got != expect {
            bail!("Sec-WebSocket-Accept mismatch: got {got}, expected {expect}");
        }

        Ok(Self {
            stream,
            buf,
            mask_seed: seed,
        })
    }

    pub fn set_timeout(&mut self, t: Duration) -> Result<()> {
        self.stream.set_read_timeout(Some(t))?;
        Ok(())
    }

    pub fn send_text(&mut self, text: &str) -> Result<()> {
        let frame = self.build_frame(0x1, text.as_bytes());
        self.stream
            .write_all(&frame)
            .context("writing a text frame")?;
        self.stream.flush()?;
        Ok(())
    }

    /// Receive one complete text message, reassembling fragments and
    /// answering control frames along the way.
    pub fn recv_text(&mut self) -> Result<String> {
        let mut message: Vec<u8> = Vec::new();
        let mut started = false;
        loop {
            let (fin, opcode, payload) = self.read_frame()?;
            match opcode {
                0x0 => {
                    if !started {
                        bail!("continuation frame with nothing to continue");
                    }
                    message.extend_from_slice(&payload);
                }
                0x1 | 0x2 => {
                    started = true;
                    message = payload;
                }
                0x8 => bail!("the server closed the WebSocket"),
                0x9 => {
                    let pong = self.build_frame(0xA, &payload);
                    self.stream.write_all(&pong)?;
                    self.stream.flush()?;
                    continue;
                }
                0xA => continue,
                other => bail!("unexpected WebSocket opcode {other:#x}"),
            }
            if message.len() > MAX_MESSAGE {
                bail!("message exceeded {MAX_MESSAGE} bytes");
            }
            if fin {
                return String::from_utf8(message).context("message is not UTF-8");
            }
        }
    }

    fn read_frame(&mut self) -> Result<(bool, u8, Vec<u8>)> {
        loop {
            if let Some(f) = take_frame(&mut self.buf)? {
                return Ok(f);
            }
            let mut c = [0u8; 16 * 1024];
            let n = self.stream.read(&mut c).context("reading a frame")?;
            if n == 0 {
                bail!("connection closed mid-frame");
            }
            self.buf.extend_from_slice(&c[..n]);
        }
    }

    fn build_frame(&mut self, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 14);
        out.push(0x80 | opcode);
        let n = payload.len();
        if n < 126 {
            out.push(0x80 | n as u8);
        } else if n <= u16::MAX as usize {
            out.push(0x80 | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            out.push(0x80 | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
        self.mask_seed = self
            .mask_seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1);
        let mask = (self.mask_seed >> 16) as u32;
        let mk = mask.to_be_bytes();
        out.extend_from_slice(&mk);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mk[i % 4]));
        out
    }
}

/// Decode one frame from the buffer. Server frames are never masked, but a
/// masked one is handled rather than silently mis-decoded.
fn take_frame(buf: &mut Vec<u8>) -> Result<Option<(bool, u8, Vec<u8>)>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let fin = buf[0] & 0x80 != 0;
    if buf[0] & 0x70 != 0 {
        bail!("reserved WebSocket bits set; no extension was negotiated");
    }
    let opcode = buf[0] & 0x0f;
    let masked = buf[1] & 0x80 != 0;
    let short = (buf[1] & 0x7f) as usize;

    let (len, mut off) = match short {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[2..10]);
            (u64::from_be_bytes(b) as usize, 10)
        }
        n => (n, 2),
    };
    if len > MAX_MESSAGE {
        bail!("frame claims {len} bytes, over the limit");
    }

    let mask = if masked {
        if buf.len() < off + 4 {
            return Ok(None);
        }
        let m = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
        off += 4;
        Some(m)
    } else {
        None
    };

    if buf.len() < off + len {
        return Ok(None);
    }
    let mut payload = buf[off..off + len].to_vec();
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    buf.drain(..off + len);
    Ok(Some((fin, opcode, payload)))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim().eq_ignore_ascii_case(name)).then(|| v.trim().to_string())
    })
}

fn seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15);
    t ^ (std::process::id() as u64) << 32 ^ 0x9E3779B97F4A7C15
}

// ------------------------------------------------- base64 and SHA-1
//
// Both are needed only for the handshake. Hand-rolling them keeps the
// dependency list honest, and both are pinned by known-answer tests below.

pub fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());

    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_the_rfc_3174_vectors() {
        assert_eq!(
            hexs(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hexs(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        // 56 bytes: exactly the padding boundary, where an off-by-one lives.
        assert_eq!(
            hexs(&sha1(&b"a".repeat(56))),
            "c2db330f6083854c99d4b5bfb6e8f29f201be699"
        );
    }

    #[test]
    fn base64_pads_correctly() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_rfc_6455_handshake_example_reproduces() {
        // RFC 6455 §1.3: key dGhlIHNhbXBsZSBub25jZQ== must accept as
        // s3pPLMBiTxaQ9kYGzzhZRbK+xOo=
        let got = b64(&sha1(
            b"dGhlIHNhbXBsZSBub25jZQ==258EAFA5-E914-47DA-95CA-C5AB0DC85B11",
        ));
        assert_eq!(got, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn client_frames_are_always_masked() {
        let mut ws = WebSocket {
            stream: dummy(),
            buf: Vec::new(),
            mask_seed: 1,
        };
        let f = ws.build_frame(0x1, b"hello");
        assert_eq!(f[0], 0x81, "FIN + text");
        assert_eq!(f[1] & 0x80, 0x80, "the mask bit is mandatory for a client");
        // The payload must not appear in the clear.
        assert!(find(&f, b"hello").is_none());
        // And it must decode back.
        let mk = [f[2], f[3], f[4], f[5]];
        let dec: Vec<u8> = f[6..]
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mk[i % 4])
            .collect();
        assert_eq!(dec, b"hello");
    }

    #[test]
    fn extended_length_thresholds_use_the_right_form() {
        let mut ws = WebSocket {
            stream: dummy(),
            buf: Vec::new(),
            mask_seed: 1,
        };
        assert_eq!(ws.build_frame(0x1, &[0u8; 125])[1] & 0x7f, 125);
        assert_eq!(ws.build_frame(0x1, &[0u8; 126])[1] & 0x7f, 126);
        assert_eq!(ws.build_frame(0x1, &vec![0u8; 65536])[1] & 0x7f, 127);
    }

    #[test]
    fn a_fragmented_server_message_reassembles() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x01, 0x03]); // text, not final
        buf.extend_from_slice(b"BiD");
        buf.extend_from_slice(&[0x80, 0x01]); // continuation, final
        buf.extend_from_slice(b"i");
        let (fin1, op1, p1) = take_frame(&mut buf).unwrap().unwrap();
        assert!(!fin1);
        assert_eq!(op1, 0x1);
        assert_eq!(p1, b"BiD");
        let (fin2, op2, p2) = take_frame(&mut buf).unwrap().unwrap();
        assert!(fin2);
        assert_eq!(op2, 0x0);
        assert_eq!(p2, b"i");
    }

    #[test]
    fn a_partial_frame_waits_instead_of_erroring() {
        let mut buf = vec![0x81, 0x05, b'h', b'e'];
        assert!(take_frame(&mut buf).unwrap().is_none());
        assert_eq!(buf.len(), 4, "nothing may be consumed from a partial frame");
    }

    #[test]
    fn reserved_bits_are_an_error_because_no_extension_was_negotiated() {
        let mut buf = vec![0xC1, 0x00];
        assert!(take_frame(&mut buf)
            .unwrap_err()
            .to_string()
            .contains("reserved"));
    }

    #[test]
    fn wss_is_refused_rather_than_downgraded() {
        let e = WebSocket::connect("wss://127.0.0.1:9222/session", Duration::from_millis(50))
            .unwrap_err()
            .to_string();
        assert!(e.contains("wss:// is not supported"), "got: {e}");
    }

    fn hexs(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn dummy() -> TcpStream {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        let s = TcpStream::connect(a).unwrap();
        drop(l);
        s
    }
}
