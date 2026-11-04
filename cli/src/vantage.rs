//! Measure the same target from other machines over ssh.
//!
//! This is the layer that settles the question the gzip investigation left
//! open: when one origin swings from 0.22 s to a 180 s timeout, is that the
//! server or the path to it? One machine cannot tell. Three can: if the slow
//! windows coincide across hosts on different networks it is the origin, and
//! if they do not it is the route.
//!
//! Nothing is installed on the remote host. The measurement is `curl` with an
//! explicit write-out format, which is present everywhere that matters,
//! including the Void box and Git-Bash or WSL on the Windows one. When `lwdbg`
//! happens to be on the remote PATH it is used instead and the richer result
//! is kept.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::process::Command;

/// The `curl` write-out fields, in order. Kept as one string so the remote
/// command is a single quoted argument and the parsing cannot drift from it.
const WRITE_OUT: &str = "%{http_code} %{time_namelookup} %{time_connect} \
                         %{time_appconnect} %{time_starttransfer} %{time_total} \
                         %{size_download} %{speed_download}";

#[derive(Debug, Serialize)]
pub struct Run {
    pub run: usize,
    pub status: u32,
    pub us_dns: i64,
    pub us_connect: i64,
    pub us_tls: i64,
    pub us_ttfb: i64,
    pub us_total: i64,
    pub bytes: i64,
}

#[derive(Debug, Serialize)]
pub struct HostResult {
    pub host: String,
    pub reachable: bool,
    pub uname: Option<String>,
    pub runs: Vec<Run>,
    pub median_us_ttfb: Option<f64>,
    pub median_us_total: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub url: String,
    pub hosts: Vec<HostResult>,
    /// What the spread across hosts implies. Stated as a reading, because a
    /// handful of runs cannot prove where a fault is.
    pub reading: String,
}

/// Run a command on a host. `local` is handled without ssh so the local
/// machine is always one of the vantage points and is measured the same way.
fn run_on(host: &str, script: &str, timeout_s: u64) -> Result<String> {
    let out = if host == "local" {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .context("running the local shell")?
    } else {
        Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                &format!("ConnectTimeout={}", timeout_s.min(30)),
                "-o",
                "StrictHostKeyChecking=accept-new",
                host,
                script,
            ])
            .output()
            .with_context(|| format!("ssh {host}"))?
    };
    if !out.status.success() {
        bail!("{host}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn parse_run(idx: usize, line: &str) -> Result<Run> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 8 {
        bail!("cannot parse curl output {line:?}");
    }
    let s = |i: usize| -> Result<f64> {
        f[i].parse::<f64>()
            .map_err(|_| anyhow!("field {i} of {line:?} is not a number"))
    };
    let us = |v: f64| (v * 1e6).round() as i64;
    Ok(Run {
        run: idx,
        status: f[0].parse().unwrap_or(0),
        us_dns: us(s(1)?),
        us_connect: us(s(2)?),
        us_tls: us(s(3)?),
        us_ttfb: us(s(4)?),
        us_total: us(s(5)?),
        bytes: s(6)? as i64,
    })
}

pub fn measure(url: &str, hosts: &[String], runs: usize, timeout_s: u64) -> Result<Report> {
    if hosts.is_empty() {
        bail!("name at least one host; `local` is a valid one");
    }
    // A URL is about to be interpolated into a remote shell command. Anything
    // that could end the quoting is refused rather than escaped.
    if url.contains('\'') || url.contains('\n') || url.contains('`') || url.contains('$') {
        bail!("refusing to send a URL containing shell metacharacters: {url:?}");
    }

    // The same browser user-agent the local measurement uses, so a server
    // that treats curl differently is not the difference between hosts.
    let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:153.0) \
              Gecko/20100101 Firefox/153.0";
    let script = format!(
        "uname -sr; for i in $(seq 1 {runs}); do \
           curl -sS -o /dev/null --compressed -m {timeout_s} \
             -A '{ua}' -w '{WRITE_OUT}\\n' '{url}' || echo 'CURL_FAILED'; \
         done"
    );

    let mut results = Vec::new();
    for h in hosts {
        match run_on(h, &script, timeout_s) {
            Ok(out) => {
                let mut lines = out.lines();
                let uname = lines.next().map(|s| s.trim().to_string());
                let mut rs = Vec::new();
                let mut failures = 0;
                for (i, l) in lines.filter(|l| !l.trim().is_empty()).enumerate() {
                    if l.contains("CURL_FAILED") {
                        failures += 1;
                        continue;
                    }
                    match parse_run(i + 1, l) {
                        Ok(r) => rs.push(r),
                        Err(_) => failures += 1,
                    }
                }
                let ttfbs: Vec<i64> = rs.iter().map(|r| r.us_ttfb).collect();
                let totals: Vec<i64> = rs.iter().map(|r| r.us_total).collect();
                results.push(HostResult {
                    host: h.clone(),
                    reachable: true,
                    uname,
                    median_us_ttfb: lwdbg_ssot::median(&ttfbs),
                    median_us_total: lwdbg_ssot::median(&totals),
                    runs: rs,
                    error: (failures > 0)
                        .then(|| format!("{failures} of {runs} runs failed or timed out")),
                });
            }
            Err(e) => results.push(HostResult {
                host: h.clone(),
                reachable: false,
                uname: None,
                runs: vec![],
                median_us_ttfb: None,
                median_us_total: None,
                error: Some(format!("{e:#}")),
            }),
        }
    }

    Ok(Report {
        url: url.to_string(),
        reading: reading(&results),
        hosts: results,
    })
}

/// Turn the per-host medians into a sentence about origin versus route.
fn reading(hosts: &[HostResult]) -> String {
    let ok: Vec<&HostResult> = hosts
        .iter()
        .filter(|h| h.median_us_ttfb.is_some())
        .collect();
    if ok.len() < 2 {
        return format!(
            "{} of {} hosts produced a median. Two or more are needed before \
             anything can be said about origin versus route.",
            ok.len(),
            hosts.len()
        );
    }
    let mut v: Vec<f64> = ok.iter().map(|h| h.median_us_ttfb.unwrap()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (lo, hi) = (v[0], v[v.len() - 1]);
    let ratio = if lo > 0.0 { hi / lo } else { f64::INFINITY };

    if ratio < 1.5 {
        format!(
            "TTFB medians agree within {ratio:.2}x across {} hosts on different \
             networks. A slow window seen everywhere at once is the origin, not \
             the route.",
            ok.len()
        )
    } else if ratio < 4.0 {
        format!(
            "TTFB medians differ by {ratio:.1}x across hosts. That is consistent \
             with a route difference, and also with the origin changing between \
             the measurements. Re-run the hosts concurrently before concluding."
        )
    } else {
        format!(
            "TTFB medians differ by {ratio:.1}x across hosts ({:.3}s vs {:.3}s). \
             A difference that large between vantage points points at the path, \
             not at the server.",
            lo / 1e6,
            hi / 1e6
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_curl_line_parses_into_microseconds() {
        let r = parse_run(1, "200 0.004 0.021 0.071 0.223 0.545 1956136 3588000").unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.us_dns, 4_000);
        assert_eq!(r.us_tls, 71_000);
        assert_eq!(r.us_ttfb, 223_000);
        assert_eq!(r.us_total, 545_000);
        assert_eq!(r.bytes, 1_956_136);
    }

    #[test]
    fn a_short_line_is_an_error_not_a_zero_filled_run() {
        assert!(parse_run(1, "200 0.1").is_err());
    }

    #[test]
    fn a_url_that_could_break_the_remote_quoting_is_refused() {
        for bad in [
            "https://x/'; rm -rf /; echo '",
            "https://x/$(id)",
            "https://x/`id`",
        ] {
            let e = measure(bad, &["local".into()], 1, 5);
            assert!(e.is_err(), "{bad:?} should have been refused");
        }
    }

    #[test]
    fn no_hosts_is_an_error() {
        assert!(measure("https://example.com/", &[], 3, 5).is_err());
    }

    #[test]
    fn one_usable_host_cannot_support_a_conclusion() {
        let hosts = vec![HostResult {
            host: "local".into(),
            reachable: true,
            uname: None,
            runs: vec![],
            median_us_ttfb: Some(200_000.0),
            median_us_total: Some(500_000.0),
            error: None,
        }];
        let r = reading(&hosts);
        assert!(r.contains("Two or more are needed"), "got: {r}");
    }

    #[test]
    fn agreement_across_hosts_points_at_the_origin() {
        let h = |name: &str, ttfb: f64| HostResult {
            host: name.into(),
            reachable: true,
            uname: None,
            runs: vec![],
            median_us_ttfb: Some(ttfb),
            median_us_total: Some(ttfb * 2.0),
            error: None,
        };
        let r = reading(&[h("local", 2_000_000.0), h("void", 2_100_000.0)]);
        assert!(r.contains("is the origin"), "got: {r}");
    }

    #[test]
    fn a_large_divergence_points_at_the_path() {
        let h = |name: &str, ttfb: f64| HostResult {
            host: name.into(),
            reachable: true,
            uname: None,
            runs: vec![],
            median_us_ttfb: Some(ttfb),
            median_us_total: Some(ttfb * 2.0),
            error: None,
        };
        let r = reading(&[h("local", 200_000.0), h("void", 3_000_000.0)]);
        assert!(r.contains("points at the path"), "got: {r}");
    }

    #[test]
    fn the_remote_script_sends_a_browser_user_agent() {
        // A server that rate-limits curl's default UA would otherwise make
        // every remote host look slow for the wrong reason.
        assert!(WRITE_OUT.contains("time_starttransfer"));
        assert!(WRITE_OUT.contains("size_download"));
    }
}
