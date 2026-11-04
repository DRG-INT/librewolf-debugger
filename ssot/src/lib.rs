//! The Debugging SSoT: one store that holds every signal, correlated by trace id.
//!
//! Bodies are never inlined in SQLite. They go to a content-addressed blob
//! directory keyed by BLAKE3, so identical payloads across runs and browsers
//! are stored once — which is also what makes render/asset parity exact rather
//! than approximate.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub const SCHEMA: &str = include_str!("../schema.sql");

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"))
}

/// Which engine family a browser belongs to. Drives what transports and
/// extension APIs are even possible — see `references/engines.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Gecko,
    Blink,
    WebKit,
    /// Pale Moon, Basilisk. No usable transport and no WebExtensions at all:
    /// proxy is the only way to observe these.
    Goanna,
    Servo,
    Ladybird,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Gecko => "gecko",
            Engine::Blink => "blink",
            Engine::WebKit => "webkit",
            Engine::Goanna => "goanna",
            Engine::Servo => "servo",
            Engine::Ladybird => "ladybird",
        }
    }

    /// Gecko speaks Marionette/RDP/BiDi; Blink speaks CDP and increasingly
    /// BiDi; WebKit has its own inspector. Goanna has none.
    pub fn has_debug_transport(self) -> bool {
        !matches!(self, Engine::Goanna)
    }

    /// A single extension artifact cannot span engines, and Goanna has no
    /// WebExtensions support whatsoever.
    pub fn supports_webextensions(self) -> bool {
        matches!(self, Engine::Gecko | Engine::Blink)
    }
}

/// How an observation was made. Recorded per trace so numbers from different
/// vantage mechanisms are never silently averaged together.
#[derive(Debug, Clone, Copy)]
pub enum Via {
    Proxy,
    Marionette,
    Rdp,
    BiDi,
    Curl,
    Extension,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Proxy => "proxy",
            Via::Marionette => "marionette",
            Via::Rdp => "rdp",
            Via::BiDi => "bidi",
            Via::Curl => "curl",
            Via::Extension => "extension",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NewTrace {
    pub target_url: String,
    pub browser: Option<String>,
    pub browser_version: Option<String>,
    pub engine: Option<String>,
    pub host: String,
    pub via: String,
    pub headless: bool,
    pub cache_mode: String,
    pub label: Option<String>,
}

/// One HTTP exchange as observed. `wire_bytes` and `decoded_bytes` are both
/// first-class because "did compression actually apply" has to be answerable
/// without re-parsing headers.
#[derive(Debug, Clone, Default)]
pub struct Exchange {
    pub seq: i64,
    pub us_offset: i64,
    pub method: String,
    pub url: String,
    pub status: Option<i64>,
    pub wire_bytes: Option<i64>,
    pub decoded_bytes: Option<i64>,
    pub content_encoding: Option<String>,
    pub us_dns: Option<i64>,
    pub us_connect: Option<i64>,
    pub us_tls: Option<i64>,
    pub us_ttfb: Option<i64>,
    pub us_total: Option<i64>,
    pub from_cache: bool,
    pub req_headers: Option<String>,
    pub res_headers: Option<String>,
    pub body_hash: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct NavigationTiming {
    pub us_ttfb: Option<i64>,
    pub us_response_end: Option<i64>,
    pub us_dom_interactive: Option<i64>,
    pub us_dcl: Option<i64>,
    pub us_load: Option<i64>,
    pub transfer_size: Option<i64>,
    pub encoded_body_size: Option<i64>,
    pub decoded_body_size: Option<i64>,
    pub dom_nodes: Option<i64>,
}

/// One entry from the Resource Timing API.
///
/// `encoded_body_size` vs `decoded_body_size` is the browser's own verdict on
/// whether compression applied, arrived at independently of what the proxy
/// saw on the wire. When the two disagree, one of them is instrumented wrong,
/// and that is worth knowing before drawing any conclusion from either.
#[derive(Debug, Clone, Default)]
pub struct ResourceTiming {
    pub url: String,
    pub initiator: Option<String>,
    pub us_start: Option<i64>,
    pub us_ttfb: Option<i64>,
    pub us_end: Option<i64>,
    pub transfer_size: Option<i64>,
    pub encoded_body_size: Option<i64>,
    pub decoded_body_size: Option<i64>,
}

/// A parsed log line. `url` is the join key against `exchange` and
/// `resource_timing`; a line without one is still stored, because the absence
/// of a request in the log is itself evidence.
#[derive(Debug, Clone, Default)]
pub struct LogLine {
    /// "moz_log" | "console" | "stderr"
    pub source: String,
    pub module: Option<String>,
    pub level: Option<i64>,
    pub us_offset: Option<i64>,
    pub url: Option<String>,
    pub message: String,
}

/// A screenshot. The pixels go to the blob store, so the same render captured
/// twice costs one copy and a parity comparison of identical output is exact.
#[derive(Debug, Clone)]
pub struct Render {
    pub width: i64,
    pub height: i64,
    pub dpr: f64,
    pub full_page: bool,
    pub blob_hash: String,
}

pub struct Store {
    conn: Connection,
    blob_dir: PathBuf,
}

impl Store {
    /// Open (creating if needed) a store rooted at `dir`:
    /// `dir/ssot.db` plus `dir/blobs/`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating store dir {}", dir.display()))?;
        let blob_dir = dir.join("blobs");
        std::fs::create_dir_all(&blob_dir)
            .with_context(|| format!("creating blob dir {}", blob_dir.display()))?;

        let conn = Connection::open(dir.join("ssot.db")).context("opening ssot.db")?;
        conn.execute_batch(SCHEMA).context("applying schema")?;
        Ok(Self { conn, blob_dir })
    }

    pub fn blob_dir(&self) -> &Path {
        &self.blob_dir
    }

    /// Store bytes by content hash. Idempotent: the same payload stored twice
    /// occupies one file and one row.
    pub fn put_blob(&self, bytes: &[u8], media_type: Option<&str>) -> Result<String> {
        let hash = blake3::hash(bytes).to_hex().to_string();

        // Fan out by first two hex chars so a large archive does not land in
        // one enormous directory.
        let (shard, rest) = hash.split_at(2);
        let rel = format!("{shard}/{rest}");
        let abs = self.blob_dir.join(&rel);

        if !abs.exists() {
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&abs, bytes)
                .with_context(|| format!("writing blob {}", abs.display()))?;
        }

        self.conn.execute(
            "INSERT INTO blob (hash, size, media_type, path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(hash) DO NOTHING",
            params![hash, bytes.len() as i64, media_type, rel, now_rfc3339()],
        )?;
        Ok(hash)
    }

    pub fn read_blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let rel: Option<String> = self
            .conn
            .query_row(
                "SELECT path FROM blob WHERE hash = ?1",
                params![hash],
                |r| r.get(0),
            )
            .optional()?;
        let Some(rel) = rel else { return Ok(None) };
        // The row can outlive the file: a pruned or hand-deleted blob dir, or
        // a store copied without its blobs. That is an absent blob, not an
        // error — callers treat it as a miss and re-fetch.
        match std::fs::read(self.blob_dir.join(rel)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading blob file"),
        }
    }

    pub fn begin_trace(&self, t: &NewTrace) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO trace
               (id, started_at, target_url, browser, browser_version, engine,
                host, via, headless, cache_mode, label)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                id,
                now_rfc3339(),
                t.target_url,
                t.browser,
                t.browser_version,
                t.engine,
                t.host,
                t.via,
                t.headless as i64,
                t.cache_mode,
                t.label,
            ],
        )?;
        Ok(id)
    }

    pub fn finish_trace(&self, trace_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE trace SET finished_at = ?2 WHERE id = ?1",
            params![trace_id, now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn record_exchange(&self, trace_id: &str, e: &Exchange) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO exchange
               (trace_id, seq, us_offset, method, url, status, wire_bytes,
                decoded_bytes, content_encoding, us_dns, us_connect, us_tls,
                us_ttfb, us_total, from_cache, req_headers, res_headers,
                body_hash, error)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            params![
                trace_id,
                e.seq,
                e.us_offset,
                e.method,
                e.url,
                e.status,
                e.wire_bytes,
                e.decoded_bytes,
                e.content_encoding,
                e.us_dns,
                e.us_connect,
                e.us_tls,
                e.us_ttfb,
                e.us_total,
                e.from_cache as i64,
                e.req_headers,
                e.res_headers,
                e.body_hash,
                e.error,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn record_navigation_timing(&self, trace_id: &str, n: &NavigationTiming) -> Result<()> {
        self.conn.execute(
            "INSERT INTO navigation_timing
               (trace_id, us_ttfb, us_response_end, us_dom_interactive, us_dcl,
                us_load, transfer_size, encoded_body_size, decoded_body_size, dom_nodes)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(trace_id) DO UPDATE SET
               us_ttfb=excluded.us_ttfb,
               us_response_end=excluded.us_response_end,
               us_dom_interactive=excluded.us_dom_interactive,
               us_dcl=excluded.us_dcl,
               us_load=excluded.us_load,
               transfer_size=excluded.transfer_size,
               encoded_body_size=excluded.encoded_body_size,
               decoded_body_size=excluded.decoded_body_size,
               dom_nodes=excluded.dom_nodes",
            params![
                trace_id,
                n.us_ttfb,
                n.us_response_end,
                n.us_dom_interactive,
                n.us_dcl,
                n.us_load,
                n.transfer_size,
                n.encoded_body_size,
                n.decoded_body_size,
                n.dom_nodes,
            ],
        )?;
        Ok(())
    }

    /// Median of a metric across traces for one target. Median rather than
    /// mean on purpose: origins throw outliers that drag a mean badly, and a
    /// single slow run is not a property of the server.
    pub fn median_us(&self, target_url: &str, metric: Metric) -> Result<Option<f64>> {
        let col = metric.column();
        let sql = format!(
            "SELECT {col} FROM navigation_timing nt
               JOIN trace t ON t.id = nt.trace_id
              WHERE t.target_url = ?1 AND {col} IS NOT NULL
              ORDER BY {col}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let vals: Vec<i64> = stmt
            .query_map(params![target_url], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(median(&vals))
    }

    /// Insert resource timings in one transaction. A page has hundreds of
    /// these; one statement each outside a transaction turns a measurement
    /// into a disk-bound operation and distorts the next run.
    pub fn record_resource_timings(
        &mut self,
        trace_id: &str,
        rows: &[ResourceTiming],
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO resource_timing
                   (trace_id, url, initiator, us_start, us_ttfb, us_end,
                    transfer_size, encoded_body_size, decoded_body_size)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            )?;
            for r in rows {
                st.execute(params![
                    trace_id,
                    r.url,
                    r.initiator,
                    r.us_start,
                    r.us_ttfb,
                    r.us_end,
                    r.transfer_size,
                    r.encoded_body_size,
                    r.decoded_body_size,
                ])?;
            }
        }
        tx.commit()?;
        Ok(rows.len())
    }

    pub fn record_log_lines(&mut self, trace_id: &str, rows: &[LogLine]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO log_line
                   (trace_id, source, module, level, us_offset, url, message)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for r in rows {
                st.execute(params![
                    trace_id,
                    r.source,
                    r.module,
                    r.level,
                    r.us_offset,
                    r.url,
                    r.message,
                ])?;
            }
        }
        tx.commit()?;
        Ok(rows.len())
    }

    pub fn record_render(&self, trace_id: &str, r: &Render) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO render
               (trace_id, width, height, dpr, full_page, blob_hash, captured_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                trace_id,
                r.width,
                r.height,
                r.dpr,
                r.full_page as i64,
                r.blob_hash,
                now_rfc3339(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Record a parity score between two renders. `backend` distinguishes the
    /// GPU and CPU paths so a disagreement between them is visible instead of
    /// overwriting the other's answer.
    pub fn record_parity(
        &self,
        render_a: i64,
        render_b: i64,
        ssim: Option<f64>,
        phash_hamming: Option<i64>,
        backend: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO parity (render_a, render_b, ssim, phash_hamming, backend, computed_at)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(render_a, render_b, backend) DO UPDATE SET
               ssim=excluded.ssim,
               phash_hamming=excluded.phash_hamming,
               computed_at=excluded.computed_at",
            params![
                render_a,
                render_b,
                ssim,
                phash_hamming,
                backend,
                now_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Metric {
    Ttfb,
    Dcl,
    Load,
}

impl Metric {
    fn column(self) -> &'static str {
        match self {
            Metric::Ttfb => "nt.us_ttfb",
            Metric::Dcl => "nt.us_dcl",
            Metric::Load => "nt.us_load",
        }
    }
}

/// Median of a sorted-or-unsorted slice. Even counts average the two middles.
pub fn median(vals: &[i64]) -> Option<f64> {
    if vals.is_empty() {
        return None;
    }
    let mut v = vals.to_vec();
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2] as f64
    } else {
        (v[n / 2 - 1] as f64 + v[n / 2] as f64) / 2.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_odd_even_and_empty() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[5]), Some(5.0));
        assert_eq!(median(&[3, 1, 2]), Some(2.0));
        assert_eq!(median(&[4, 1, 3, 2]), Some(2.5));
    }

    #[test]
    fn median_ignores_a_single_outlier() {
        // The 122s outlier observed against a real origin must not move the
        // reported figure the way a mean would.
        let tight = [3_200_000i64, 4_700_000, 5_700_000];
        let with_outlier = [3_200_000i64, 4_700_000, 5_700_000, 122_000_000];
        assert_eq!(median(&tight), Some(4_700_000.0));
        assert_eq!(median(&with_outlier), Some(5_200_000.0));
    }

    #[test]
    fn engine_capabilities_match_the_matrix() {
        assert!(!Engine::Goanna.supports_webextensions());
        assert!(!Engine::Goanna.has_debug_transport());
        assert!(Engine::Gecko.supports_webextensions());
        assert!(Engine::Blink.supports_webextensions());
        assert!(!Engine::WebKit.supports_webextensions());
    }

    #[test]
    fn blob_store_is_content_addressed_and_idempotent() {
        // Test helper. The name carries the pid and the directory is
        // removed first; nothing secret is written here, and a test that
        // could not use a temp dir would need a fixture in the repo.
        // nosemgrep: rust.lang.security.temp-dir.temp-dir
        let dir = std::env::temp_dir().join(format!("lwdbg-test-{}", uuid::Uuid::new_v4()));
        let s = Store::open(&dir).unwrap();
        let h1 = s.put_blob(b"hello", Some("text/plain")).unwrap();
        let h2 = s.put_blob(b"hello", Some("text/plain")).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(s.read_blob(&h1).unwrap().unwrap(), b"hello");
        let n: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM blob", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn trace_and_exchange_round_trip() {
        // Test helper. The name carries the pid and the directory is
        // removed first; nothing secret is written here, and a test that
        // could not use a temp dir would need a fixture in the repo.
        // nosemgrep: rust.lang.security.temp-dir.temp-dir
        let dir = std::env::temp_dir().join(format!("lwdbg-test-{}", uuid::Uuid::new_v4()));
        let s = Store::open(&dir).unwrap();
        let id = s
            .begin_trace(&NewTrace {
                target_url: "https://example.invalid/".into(),
                host: "local".into(),
                via: Via::Proxy.as_str().into(),
                cache_mode: "live".into(),
                ..Default::default()
            })
            .unwrap();
        s.record_exchange(
            &id,
            &Exchange {
                seq: 0,
                us_offset: 0,
                method: "GET".into(),
                url: "https://example.invalid/".into(),
                status: Some(200),
                wire_bytes: Some(1_956_136),
                decoded_bytes: Some(3_076_779),
                content_encoding: Some("gzip".into()),
                ..Default::default()
            },
        )
        .unwrap();
        s.finish_trace(&id).unwrap();

        // The compression view must answer the gzip question directly.
        let (compressed, saved): (i64, f64) = s
            .conn()
            .query_row(
                "SELECT compressed, saved_fraction FROM v_compression WHERE trace_id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(compressed, 1);
        assert!((saved - 0.364).abs() < 0.01, "saved was {saved}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
