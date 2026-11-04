//! Content-encoding handling.
//!
//! The proxy keeps bodies **as they arrived on the wire** and decodes only to
//! learn the decoded size. Re-encoding would change what the client receives
//! and would make the wire figure a fiction — and the wire-versus-decoded pair
//! is precisely the evidence needed to answer "did compression actually apply".

use anyhow::{bail, Result};
use std::io::Read;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Identity,
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

impl Encoding {
    /// Parse a `Content-Encoding` value. Only a single encoding is handled;
    /// a chain such as `gzip, br` returns `None` so the caller records the
    /// decoded size as unknown rather than reporting a wrong number.
    pub fn parse(value: &str) -> Option<Self> {
        let v = value.trim().to_ascii_lowercase();
        if v.contains(',') {
            return None;
        }
        Some(match v.as_str() {
            "" | "identity" => Encoding::Identity,
            "gzip" | "x-gzip" => Encoding::Gzip,
            "deflate" => Encoding::Deflate,
            "br" => Encoding::Brotli,
            "zstd" => Encoding::Zstd,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Identity => "identity",
            Encoding::Gzip => "gzip",
            Encoding::Deflate => "deflate",
            Encoding::Brotli => "br",
            Encoding::Zstd => "zstd",
        }
    }
}

/// Cap on decompressed output. A hostile or broken origin can send a small
/// body that expands without bound; this is a measurement tool, so it refuses
/// rather than exhausting memory.
pub const MAX_DECODED: usize = 256 * 1024 * 1024;

pub fn decode(enc: Encoding, body: &[u8]) -> Result<Vec<u8>> {
    let out = match enc {
        Encoding::Identity => body.to_vec(),
        Encoding::Gzip => {
            let mut d = flate2::read::GzDecoder::new(body);
            read_capped(&mut d)?
        }
        Encoding::Deflate => {
            let mut d = flate2::read::ZlibDecoder::new(body);
            match read_capped(&mut d) {
                Ok(v) => v,
                // Some servers send raw deflate under the `deflate` name.
                Err(_) => {
                    let mut d = flate2::read::DeflateDecoder::new(body);
                    read_capped(&mut d)?
                }
            }
        }
        Encoding::Brotli => {
            let mut d = brotli::Decompressor::new(body, 4096);
            read_capped(&mut d)?
        }
        Encoding::Zstd => {
            let mut d = zstd::stream::read::Decoder::new(body)?;
            read_capped(&mut d)?
        }
    };
    Ok(out)
}

fn read_capped(r: &mut impl Read) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if out.len() + n > MAX_DECODED {
            bail!("decoded body exceeds {MAX_DECODED} bytes; refusing to continue");
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// Decoded size, or `None` when the encoding is unrecognised or decoding
/// fails. `None` means "not known", never "same as wire".
pub fn decoded_len(content_encoding: Option<&str>, body: &[u8]) -> Option<usize> {
    let enc = Encoding::parse(content_encoding.unwrap_or("identity"))?;
    if enc == Encoding::Identity {
        return Some(body.len());
    }
    decode(enc, body).ok().map(|v| v.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_known_encodings_and_rejects_chains() {
        assert_eq!(Encoding::parse("gzip"), Some(Encoding::Gzip));
        assert_eq!(Encoding::parse("  BR "), Some(Encoding::Brotli));
        assert_eq!(Encoding::parse(""), Some(Encoding::Identity));
        assert_eq!(Encoding::parse("gzip, br"), None, "chains must be unknown");
        assert_eq!(Encoding::parse("exotic"), None);
    }

    #[test]
    fn gzip_round_trip_reports_both_sizes() {
        let plain = b"x".repeat(100_000);
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(&plain).unwrap();
        let wire = e.finish().unwrap();

        assert!(wire.len() < plain.len(), "compression should shrink this");
        assert_eq!(decoded_len(Some("gzip"), &wire), Some(plain.len()));
        // The saving is what v_compression reports; check it is sane.
        let saved = 1.0 - (wire.len() as f64 / plain.len() as f64);
        assert!(saved > 0.9, "saved was {saved}");
    }

    #[test]
    fn identity_decoded_len_is_the_wire_len() {
        assert_eq!(decoded_len(None, b"hello"), Some(5));
        assert_eq!(decoded_len(Some("identity"), b"hello"), Some(5));
    }

    #[test]
    fn unknown_encoding_is_none_not_a_guess() {
        // Reporting wire length here would silently claim "no compression".
        assert_eq!(decoded_len(Some("gzip, br"), b"whatever"), None);
        assert_eq!(decoded_len(Some("made-up"), b"whatever"), None);
    }

    #[test]
    fn corrupt_gzip_is_none_not_an_error_path_crash() {
        assert_eq!(decoded_len(Some("gzip"), b"not actually gzip"), None);
    }
}
