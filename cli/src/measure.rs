//! Measurement.
//!
//! Three runs and a median by default, with the spread reported alongside.
//! A single observation is not a property of a server: real origins have been
//! seen to vary from 3s to 9s on the same request, with one 122s outlier that
//! a mean would have swallowed and a median does not.

use anyhow::{anyhow, Context, Result};
use lwdbg_ssot::{median, Exchange, NewTrace, Store, Via};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Clone)]
pub struct Run {
    pub run: usize,
    pub us_dns: i64,
    pub us_connect: i64,
    pub us_tls: i64,
    pub us_ttfb: i64,
    pub us_total: i64,
    pub wire_bytes: i64,
    pub status: i64,
    pub content_encoding: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub url: String,
    pub via: String,
    pub runs: Vec<Run>,
    pub median_us_ttfb: Option<f64>,
    pub median_us_total: Option<f64>,
    pub min_us_total: Option<i64>,
    pub max_us_total: Option<i64>,
    /// Max/min of total time. A high number means the origin is erratic and a
    /// single figure would misrepresent it.
    pub spread_ratio: Option<f64>,
    pub compressed: Option<bool>,
    pub note: String,
}

/// One curl run with `--compressed`, parsed from a machine-readable `-w`.
fn curl_once(url: &str, compressed: bool) -> Result<Run> {
    let fmt = "%{http_code} %{time_namelookup} %{time_connect} %{time_appconnect} \
               %{time_starttransfer} %{time_total} %{size_download} %{content_type}";
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "-o", "/dev/null", "-w", fmt, "--max-time", "120"]);
    if compressed {
        cmd.arg("--compressed");
    }
    // A browser-like UA: some origins treat an unadorned curl differently, and
    // the baseline should be comparable to the browser path.
    cmd.args([
        "-A",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/131.0.0.0 Safari/537.36",
    ]);
    cmd.arg(url);

    let out = cmd.output().context("running curl")?;
    if !out.status.success() {
        return Err(anyhow!(
            "curl failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let f: Vec<&str> = s.split_whitespace().collect();
    if f.len() < 7 {
        return Err(anyhow!("unparsable curl output: {s:?}"));
    }
    let secs = |i: usize| -> i64 { (f[i].parse::<f64>().unwrap_or(0.0) * 1_000_000.0) as i64 };
    Ok(Run {
        run: 0,
        status: f[0].parse().unwrap_or(0),
        us_dns: secs(1),
        us_connect: secs(2),
        us_tls: secs(3),
        us_ttfb: secs(4),
        us_total: secs(5),
        wire_bytes: f[6].parse().unwrap_or(0),
        content_encoding: None,
    })
}

pub fn curl_baseline(store: &Store, url: &str, runs: usize, compressed: bool) -> Result<Summary> {
    let trace_id = store.begin_trace(&NewTrace {
        target_url: url.to_string(),
        browser: Some("curl".into()),
        host: "local".into(),
        via: Via::Curl.as_str().into(),
        cache_mode: "bypass".into(),
        label: Some("curl baseline".into()),
        ..Default::default()
    })?;

    let mut out = Vec::new();
    for i in 0..runs {
        let mut r = curl_once(url, compressed)?;
        r.run = i + 1;
        store.record_exchange(
            &trace_id,
            &Exchange {
                seq: i as i64,
                us_offset: 0,
                method: "GET".into(),
                url: url.to_string(),
                status: Some(r.status),
                wire_bytes: Some(r.wire_bytes),
                us_dns: Some(r.us_dns),
                us_connect: Some(r.us_connect),
                us_tls: Some(r.us_tls),
                us_ttfb: Some(r.us_ttfb),
                us_total: Some(r.us_total),
                req_headers: serde_json::to_string(&BTreeMap::from([(
                    "accept-encoding".to_string(),
                    if compressed {
                        "gzip, deflate, br"
                    } else {
                        "identity"
                    }
                    .to_string(),
                )]))
                .ok(),
                ..Default::default()
            },
        )?;
        out.push(r);
    }
    store.finish_trace(&trace_id)?;

    let ttfbs: Vec<i64> = out.iter().map(|r| r.us_ttfb).collect();
    let totals: Vec<i64> = out.iter().map(|r| r.us_total).collect();
    let min = totals.iter().copied().min();
    let max = totals.iter().copied().max();
    let spread = match (min, max) {
        (Some(mn), Some(mx)) if mn > 0 => Some(mx as f64 / mn as f64),
        _ => None,
    };

    let note = match spread {
        Some(s) if s >= 2.0 => format!(
            "origin is erratic: slowest run was {s:.1}x the fastest. Report the median with \
             the range, not a single figure."
        ),
        Some(_) => "origin was consistent across runs.".to_string(),
        None => String::new(),
    };

    Ok(Summary {
        url: url.to_string(),
        via: "curl".into(),
        runs: out,
        median_us_ttfb: median(&ttfbs),
        median_us_total: median(&totals),
        min_us_total: min,
        max_us_total: max,
        spread_ratio: spread,
        compressed: Some(compressed),
        note,
    })
}
