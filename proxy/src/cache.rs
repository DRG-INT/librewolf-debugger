//! Content-addressed response cache.
//!
//! Bodies go to the SSoT blob store, so a payload that is both cached and
//! captured is stored once. The index lives in `cache_entry`.
//!
//! Caching exists here to make a slow origin affordable to measure repeatedly.
//! It does **not** make a cached load equivalent to a real visitor's load, and
//! every trace records its `cache_mode` so the two are never conflated in a
//! report.

use anyhow::{Context, Result};
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use lwdbg_ssot::Store;
use rusqlite::{params, OptionalExtension};
use std::collections::BTreeMap;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// Owns its `Store` outright rather than sharing an `Arc`: `rusqlite`'s
/// `Connection` is `Send` but not `Sync`, so `Arc<Store>` could never cross
/// threads. The proxy wraps the whole `Cache` in a `Mutex` instead, which is
/// `Sync` because `Cache` is `Send`.
pub struct Cache {
    store: Store,
}

#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub wire_bytes: i64,
    pub decoded_bytes: Option<i64>,
    pub content_encoding: Option<String>,
    pub fresh: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Derive the cache key.
///
/// The request headers named by the response's `Vary` are folded in. Without
/// this a gzip body can be handed to a client that never asked for one — and
/// since content-encoding is exactly what this tool is often measuring,
/// getting it wrong would corrupt the finding, not just the cache.
pub fn derive_key(method: &str, url: &str, vary: Option<&str>, req_headers: &HeaderMap) -> String {
    let mut h = blake3::Hasher::new();
    h.update(method.as_bytes());
    h.update(b"\0");
    h.update(url.as_bytes());

    if let Some(vary) = vary {
        // Sort and lowercase so header order never changes the key.
        let mut names: Vec<String> = vary
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty() && s != "*")
            .collect();
        names.sort();
        names.dedup();
        for name in names {
            h.update(b"\0");
            h.update(name.as_bytes());
            h.update(b"=");
            if let Ok(hn) = HeaderName::try_from(name.as_str()) {
                // Join multiple values rather than taking the first, or two
                // genuinely different requests could collide.
                let joined: Vec<&str> = req_headers
                    .get_all(&hn)
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .collect();
                h.update(joined.join(",").as_bytes());
            }
        }
    }
    h.finalize().to_hex().to_string()
}

/// `Vary: *` means the response is never reusable from cache.
pub fn vary_forbids_caching(vary: Option<&str>) -> bool {
    vary.map(|v| v.split(',').any(|s| s.trim() == "*"))
        .unwrap_or(false)
}

/// Freshness lifetime from `Cache-Control: max-age`, else `Expires`.
/// Returns `None` when the response says nothing, which this cache treats as
/// "must revalidate" rather than "fresh forever".
pub fn freshness(headers: &BTreeMap<String, String>) -> Option<OffsetDateTime> {
    if let Some(cc) = headers.get("cache-control") {
        let cc = cc.to_ascii_lowercase();
        if cc.contains("no-store") || cc.contains("no-cache") {
            return None;
        }
        for part in cc.split(',') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("max-age=") {
                if let Ok(secs) = rest.trim().parse::<i64>() {
                    return Some(OffsetDateTime::now_utc() + Duration::seconds(secs));
                }
            }
        }
    }
    if let Some(exp) = headers.get("expires") {
        if let Ok(t) = OffsetDateTime::parse(exp, &Rfc3339) {
            return Some(t);
        }
    }
    None
}

/// Whether a response may be stored at all.
pub fn is_storable(method: &str, status: u16, headers: &BTreeMap<String, String>) -> bool {
    if !matches!(method, "GET" | "HEAD") {
        return false;
    }
    if let Some(cc) = headers.get("cache-control") {
        let cc = cc.to_ascii_lowercase();
        if cc.contains("no-store") || cc.contains("private") {
            return false;
        }
    }
    if vary_forbids_caching(headers.get("vary").map(|s| s.as_str())) {
        return false;
    }
    // 206 is excluded deliberately: a cached partial would be served as if
    // complete, which is worse than not caching.
    matches!(
        status,
        200 | 203 | 204 | 300 | 301 | 404 | 405 | 410 | 414 | 501
    )
}

/// One row of `cache_entry` as `lookup` selects it.
///
/// Named rather than spelled out at the call site: the tuple has ten
/// fields and the column order has to match the SELECT exactly, so a
/// reader needs to see them as one thing.
type CacheRow = (
    i64,
    String,
    String,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

impl Cache {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn lookup(&self, key: &str) -> Result<Option<CachedResponse>> {
        let row: Option<CacheRow> = self
            .store
            .conn()
            .query_row(
                "SELECT status, res_headers, body_hash, wire_bytes, decoded_bytes,
                        content_encoding, expires_at, etag, last_modified
                   FROM cache_entry WHERE key = ?1",
                params![key],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .optional()?;

        let Some((status, headers_json, body_hash, wire, decoded, enc, expires, etag, lastmod)) =
            row
        else {
            return Ok(None);
        };

        let Some(body) = self.store.read_blob(&body_hash)? else {
            // Index and blob store disagree. Treat as a miss and drop the row
            // rather than serving nothing.
            self.store
                .conn()
                .execute("DELETE FROM cache_entry WHERE key = ?1", params![key])?;
            return Ok(None);
        };

        let fresh = expires
            .as_deref()
            .and_then(|e| OffsetDateTime::parse(e, &Rfc3339).ok())
            .map(|t| t > OffsetDateTime::now_utc())
            .unwrap_or(false);

        self.store.conn().execute(
            "UPDATE cache_entry SET hit_count = hit_count + 1 WHERE key = ?1",
            params![key],
        )?;

        Ok(Some(CachedResponse {
            status: status as u16,
            headers: serde_json::from_str(&headers_json).unwrap_or_default(),
            body,
            wire_bytes: wire,
            decoded_bytes: decoded,
            content_encoding: enc,
            fresh,
            etag,
            last_modified: lastmod,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn put(
        &self,
        key: &str,
        method: &str,
        url: &str,
        status: u16,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<()> {
        let media = headers.get("content-type").cloned();
        let hash = self
            .store
            .put_blob(body, media.as_deref())
            .context("storing cached body")?;

        let enc = headers.get("content-encoding").cloned();
        let decoded = crate::codec::decoded_len(enc.as_deref(), body).map(|v| v as i64);
        let expires = freshness(headers).and_then(|t| t.format(&Rfc3339).ok());

        self.store.conn().execute(
            "INSERT INTO cache_entry
               (key, method, url, status, res_headers, body_hash, wire_bytes,
                decoded_bytes, content_encoding, vary, etag, last_modified,
                stored_at, expires_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(key) DO UPDATE SET
               status=excluded.status, res_headers=excluded.res_headers,
               body_hash=excluded.body_hash, wire_bytes=excluded.wire_bytes,
               decoded_bytes=excluded.decoded_bytes,
               content_encoding=excluded.content_encoding,
               etag=excluded.etag, last_modified=excluded.last_modified,
               stored_at=excluded.stored_at, expires_at=excluded.expires_at",
            params![
                key,
                method,
                url,
                status as i64,
                serde_json::to_string(headers)?,
                hash,
                body.len() as i64,
                decoded,
                enc,
                headers.get("vary"),
                headers.get("etag"),
                headers.get("last-modified"),
                OffsetDateTime::now_utc().format(&Rfc3339)?,
                expires,
            ],
        )?;
        Ok(())
    }

    /// Headers to add to a conditional revalidation request for a stale entry.
    pub fn revalidation_headers(c: &CachedResponse) -> Vec<(HeaderName, HeaderValue)> {
        let mut out = Vec::new();
        if let Some(etag) = &c.etag {
            if let Ok(v) = HeaderValue::from_str(etag) {
                out.push((hyper::header::IF_NONE_MATCH, v));
            }
        }
        if let Some(lm) = &c.last_modified {
            if let Ok(v) = HeaderValue::from_str(lm) {
                out.push((hyper::header::IF_MODIFIED_SINCE, v));
            }
        }
        out
    }
}

pub fn headers_to_map(h: &HeaderMap) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for (k, v) in h.iter() {
        if let Ok(s) = v.to_str() {
            m.entry(k.as_str().to_ascii_lowercase())
                .and_modify(|e: &mut String| {
                    e.push_str(", ");
                    e.push_str(s);
                })
                .or_insert_with(|| s.to_string());
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::try_from(*k).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn key_ignores_headers_not_named_by_vary() {
        let a = derive_key(
            "GET",
            "https://x/",
            None,
            &hm(&[("accept-encoding", "gzip")]),
        );
        let b = derive_key("GET", "https://x/", None, &hm(&[("accept-encoding", "br")]));
        assert_eq!(a, b, "without Vary, request headers must not split the key");
    }

    #[test]
    fn key_splits_on_a_varied_header() {
        let v = Some("Accept-Encoding");
        let a = derive_key("GET", "https://x/", v, &hm(&[("accept-encoding", "gzip")]));
        let b = derive_key("GET", "https://x/", v, &hm(&[("accept-encoding", "br")]));
        assert_ne!(
            a, b,
            "a gzip body must not be servable to a client that asked for br"
        );
    }

    #[test]
    fn key_is_insensitive_to_vary_order_and_case() {
        let h = hm(&[("accept-encoding", "gzip"), ("accept-language", "hu")]);
        let a = derive_key(
            "GET",
            "https://x/",
            Some("Accept-Encoding, Accept-Language"),
            &h,
        );
        let b = derive_key(
            "GET",
            "https://x/",
            Some("accept-language,accept-encoding"),
            &h,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn vary_star_is_never_cacheable() {
        assert!(vary_forbids_caching(Some("*")));
        assert!(vary_forbids_caching(Some("Accept-Encoding, *")));
        assert!(!vary_forbids_caching(Some("Accept-Encoding")));
        assert!(!is_storable("GET", 200, &map(&[("vary", "*")])));
    }

    #[test]
    fn storability_rules() {
        assert!(is_storable("GET", 200, &map(&[])));
        assert!(!is_storable("POST", 200, &map(&[])));
        assert!(!is_storable(
            "GET",
            200,
            &map(&[("cache-control", "no-store")])
        ));
        assert!(!is_storable(
            "GET",
            200,
            &map(&[("cache-control", "private")])
        ));
        // A partial served as if complete would be worse than a miss.
        assert!(!is_storable("GET", 206, &map(&[])));
        assert!(!is_storable("GET", 500, &map(&[])));
    }

    #[test]
    fn missing_freshness_info_is_not_fresh_forever() {
        assert!(freshness(&map(&[])).is_none());
        assert!(freshness(&map(&[("cache-control", "no-cache")])).is_none());
        assert!(freshness(&map(&[("cache-control", "max-age=60")])).is_some());
    }

    #[test]
    fn round_trip_through_the_store() {
        // Test helper. The name carries the pid and the directory is
        // removed first; nothing secret is written here, and a test that
        // could not use a temp dir would need a fixture in the repo.
        // nosemgrep: rust.lang.security.temp-dir.temp-dir
        let dir = std::env::temp_dir().join(format!("lwdbg-cache-{}", uuid_like()));
        let cache = Cache::new(Store::open(&dir).unwrap());

        let headers = map(&[
            ("content-type", "text/html"),
            ("content-encoding", "gzip"),
            ("cache-control", "max-age=600"),
            ("etag", "\"abc\""),
        ]);
        // A real gzip payload so decoded_bytes is genuinely computed.
        use std::io::Write;
        let plain = b"y".repeat(50_000);
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(&plain).unwrap();
        let wire = e.finish().unwrap();

        let key = derive_key("GET", "https://x/p", None, &HeaderMap::new());
        cache
            .put(&key, "GET", "https://x/p", 200, &headers, &wire)
            .unwrap();

        let got = cache.lookup(&key).unwrap().unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(got.body, wire, "the wire bytes must come back unchanged");
        assert_eq!(got.decoded_bytes, Some(plain.len() as i64));
        assert!(got.fresh, "max-age=600 should still be fresh");
        assert_eq!(got.etag.as_deref(), Some("\"abc\""));
        assert!(!Cache::revalidation_headers(&got).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_blob_vanishing_under_the_index_is_a_miss_not_a_panic() {
        // Test helper. The name carries the pid and the directory is
        // removed first; nothing secret is written here, and a test that
        // could not use a temp dir would need a fixture in the repo.
        // nosemgrep: rust.lang.security.temp-dir.temp-dir
        let dir = std::env::temp_dir().join(format!("lwdbg-cache-{}", uuid_like()));
        let cache = Cache::new(Store::open(&dir).unwrap());
        let key = derive_key("GET", "https://x/q", None, &HeaderMap::new());
        cache
            .put(&key, "GET", "https://x/q", 200, &map(&[]), b"body")
            .unwrap();
        std::fs::remove_dir_all(cache.store().blob_dir()).unwrap();
        assert!(cache.lookup(&key).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn uuid_like() -> String {
        format!(
            "{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }
}
