//! Measure a page load through a real browser, over any of the three Gecko
//! transports, and land every signal in the SSoT under one trace id.
//!
//! The same measurement is defined once and executed three ways, because the
//! transports disagree about what they can do:
//!
//! * **Marionette** has a real navigate-and-wait command. Cleanest path.
//! * **BiDi** has `browsingContext.navigate` with `wait: complete`. Also
//!   clean, and the only one of the three a Blink browser will also answer.
//! * **RDP** has no version-stable navigate command, so navigation is done by
//!   assigning `location.href` from the tab's console and then polling
//!   `document.readyState`. That is uglier, and it is chosen deliberately:
//!   RDP is the transport that reaches an **already-running** browser, where
//!   restarting to get a nicer API defeats the purpose.
//!
//! All three read the *same* two scripts, so the numbers are comparable. If
//! they ever disagree, the transport is the variable, not the definition.

use anyhow::{anyhow, bail, Context, Result};
use lwdbg_ssot::{NavigationTiming, NewTrace, Render, ResourceTiming, Store};
use lwdbg_transports::{
    bidi::Bidi,
    launch,
    marionette::{self, Marionette},
    mozlog,
    rdp::Rdp,
    Transport,
};
use serde::Serialize;
use serde_json::Value;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Opts {
    pub binary: PathBuf,
    pub browser_name: String,
    pub url: String,
    pub transport: Transport,
    pub headless: bool,
    pub runs: usize,
    pub proxy: Option<(String, u16)>,
    pub moz_log: Option<String>,
    pub profile_root: PathBuf,
    pub screenshot: bool,
    pub full_page: bool,
    pub keep_profile: bool,
    pub width: u32,
    pub height: u32,
    pub label: Option<String>,
    /// How long to wait for the transport port, and for a navigation.
    pub timeout: Duration,
}

#[derive(Debug, Serialize)]
pub struct RunResult {
    pub run: usize,
    pub trace_id: String,
    pub us_ttfb: Option<i64>,
    pub us_dcl: Option<i64>,
    pub us_load: Option<i64>,
    pub encoded_body_size: Option<i64>,
    pub decoded_body_size: Option<i64>,
    pub resources: usize,
    pub log_lines: usize,
    pub render_id: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub url: String,
    pub browser: String,
    pub via: String,
    pub headless: bool,
    pub runs: Vec<RunResult>,
    pub median_us_ttfb: Option<f64>,
    pub median_us_load: Option<f64>,
    /// Slowest total over fastest. A high value means the origin, not the
    /// browser, is the thing being measured.
    pub spread_ratio: Option<f64>,
    pub note: String,
}

/// A loopback port nothing is listening on.
///
/// Binding and immediately releasing leaves a window in which something else
/// could take the port. There is no way to hand a bound socket to a browser
/// that opens its own listener, so the window is unavoidable; it is seconds
/// wide and the failure is a clean "port never accepted", not a wrong
/// measurement.
fn free_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0").context("reserving a loopback port")?;
    let p = l.local_addr()?.port();
    drop(l);
    Ok(p)
}

pub fn measure(store: &mut Store, o: &Opts) -> Result<Summary> {
    if o.runs == 0 {
        bail!("--runs 0 measures nothing");
    }
    let binary = launch::resolve_binary(&o.binary)?;
    let version = read_version(&binary);

    let mut runs = Vec::new();
    for i in 1..=o.runs {
        match one_run(store, o, &binary, version.as_deref(), i) {
            Ok(r) => runs.push(r),
            Err(e) => runs.push(RunResult {
                run: i,
                trace_id: String::new(),
                us_ttfb: None,
                us_dcl: None,
                us_load: None,
                encoded_body_size: None,
                decoded_body_size: None,
                resources: 0,
                log_lines: 0,
                render_id: None,
                error: Some(format!("{e:#}")),
            }),
        }
    }

    let ttfbs: Vec<i64> = runs.iter().filter_map(|r| r.us_ttfb).collect();
    let loads: Vec<i64> = runs.iter().filter_map(|r| r.us_load).collect();
    let spread = match (loads.iter().min(), loads.iter().max()) {
        (Some(&mn), Some(&mx)) if mn > 0 => Some(mx as f64 / mn as f64),
        _ => None,
    };

    let ok = runs.iter().filter(|r| r.error.is_none()).count();
    let mut note = String::new();
    if ok < o.runs {
        note.push_str(&format!("{} of {} runs failed. ", o.runs - ok, o.runs));
    }
    if ok == 1 {
        note.push_str(
            "One successful run is not a measurement: this origin has been seen \
             to vary by two orders of magnitude. ",
        );
    }
    if let Some(s) = spread {
        if s >= 2.0 {
            note.push_str(&format!(
                "Load time varied {s:.1}x across runs; the origin is erratic and \
                 the median is the only reportable number. "
            ));
        }
    }

    Ok(Summary {
        url: o.url.clone(),
        browser: o.browser_name.clone(),
        via: o.transport.as_str().to_string(),
        headless: o.headless,
        runs,
        median_us_ttfb: lwdbg_ssot::median(&ttfbs),
        median_us_load: lwdbg_ssot::median(&loads),
        spread_ratio: spread,
        note: note.trim().to_string(),
    })
}

fn one_run(
    store: &mut Store,
    o: &Opts,
    binary: &Path,
    version: Option<&str>,
    run: usize,
) -> Result<RunResult> {
    let profile = o.profile_root.join(format!(
        "{}-{}-run{run}",
        o.transport.as_str(),
        std::process::id()
    ));
    let log_file = o.moz_log.as_ref().map(|_| profile.with_extension("log"));

    let port = free_port()?;
    let spec = launch::LaunchSpec {
        binary: binary.to_path_buf(),
        profile_dir: profile.clone(),
        headless: o.headless,
        marionette_port: (o.transport == Transport::Marionette).then_some(port),
        rdp_port: (o.transport == Transport::Rdp).then_some(port),
        bidi_port: (o.transport == Transport::Bidi).then_some(port),
        proxy: o.proxy.as_ref().map(|(h, p)| launch::ProxyPrefs {
            host: h.clone(),
            port: *p,
            include_localhost: true,
        }),
        moz_log: o.moz_log.clone(),
        log_file: log_file.clone(),
        extra_args: vec![],
        // Navigating from about:blank, rather than starting on the target,
        // keeps the Navigation Timing entry about the page under test and not
        // about startup.
        url: None,
    };

    let trace_id = store.begin_trace(&NewTrace {
        target_url: o.url.clone(),
        browser: Some(o.browser_name.clone()),
        browser_version: version.map(str::to_string),
        engine: Some("gecko".into()),
        host: "local".into(),
        via: o.transport.as_str().to_string(),
        headless: o.headless,
        cache_mode: if o.proxy.is_some() {
            "live".into()
        } else {
            "bypass".into()
        },
        label: o.label.clone(),
    })?;

    let mut child = launch::spawn(spec)?;
    let started = Instant::now();
    let outcome = (|| -> Result<(Value, Vec<Value>, Option<String>)> {
        launch::wait_for_port(port, o.timeout)?;
        match o.transport {
            Transport::Marionette => drive_marionette(o, port),
            Transport::Bidi => drive_bidi(o, port),
            Transport::Rdp => drive_rdp(o, port),
        }
    })();
    let elapsed = started.elapsed();
    child.shutdown(Duration::from_secs(5)).ok();

    // Logs are read after shutdown: Gecko flushes on exit and a tail read
    // while it is still writing loses the last records.
    let mut log_lines = 0usize;
    if let Some(f) = &log_file {
        if let Ok(text) = std::fs::read_to_string(f) {
            let rows: Vec<lwdbg_ssot::LogLine> = mozlog::parse(&text)
                .into_iter()
                .map(|l| lwdbg_ssot::LogLine {
                    source: "moz_log".into(),
                    module: l.module,
                    level: l.level,
                    us_offset: None,
                    url: l.url,
                    message: l.message,
                })
                .collect();
            log_lines = store.record_log_lines(&trace_id, &rows)?;
        }
    }

    let (nav, resources, shot) = match outcome {
        Ok(v) => v,
        Err(e) => {
            store.finish_trace(&trace_id)?;
            if !o.keep_profile {
                std::fs::remove_dir_all(&profile).ok();
            }
            return Ok(RunResult {
                run,
                trace_id,
                us_ttfb: None,
                us_dcl: None,
                us_load: None,
                encoded_body_size: None,
                decoded_body_size: None,
                resources: 0,
                log_lines,
                render_id: None,
                error: Some(format!("{e:#} (after {:.1}s)", elapsed.as_secs_f64())),
            });
        }
    };

    let nt = nav_timing_from(&nav);
    store.record_navigation_timing(&trace_id, &nt)?;

    let rows: Vec<ResourceTiming> = resources.iter().map(resource_from).collect();
    let n_res = store.record_resource_timings(&trace_id, &rows)?;

    let render_id = match shot {
        Some(b64png) => {
            let bytes = decode_b64(&b64png)?;
            let (w, h) = png_dimensions(&bytes).unwrap_or((0, 0));
            let hash = store.put_blob(&bytes, Some("image/png"))?;
            Some(store.record_render(
                &trace_id,
                &Render {
                    width: w as i64,
                    height: h as i64,
                    dpr: 1.0,
                    full_page: o.full_page,
                    blob_hash: hash,
                },
            )?)
        }
        None => None,
    };

    store.finish_trace(&trace_id)?;
    if !o.keep_profile {
        std::fs::remove_dir_all(&profile).ok();
    }

    Ok(RunResult {
        run,
        trace_id,
        us_ttfb: nt.us_ttfb,
        us_dcl: nt.us_dcl,
        us_load: nt.us_load,
        encoded_body_size: nt.encoded_body_size,
        decoded_body_size: nt.decoded_body_size,
        resources: n_res,
        log_lines,
        render_id,
        error: None,
    })
}

// ------------------------------------------------------------- the three drivers

fn drive_marionette(o: &Opts, port: u16) -> Result<(Value, Vec<Value>, Option<String>)> {
    let addr = format!("127.0.0.1:{port}");
    let mut m = Marionette::connect(&addr, o.timeout)?;
    m.new_session()?;
    let ms = o.timeout.as_millis() as u64;
    m.set_timeouts(ms, ms)?;
    if !o.headless {
        m.set_window_rect(o.width, o.height).ok();
    }
    m.navigate(&o.url)?;
    let nav = m.execute(marionette::NAV_TIMING_SCRIPT)?;
    let res = m.execute(marionette::RESOURCE_TIMING_SCRIPT)?;
    let shot = if o.screenshot {
        Some(m.screenshot(o.full_page)?)
    } else {
        None
    };
    m.delete_session().ok();
    Ok((nav, res.as_array().cloned().unwrap_or_default(), shot))
}

fn drive_bidi(o: &Opts, port: u16) -> Result<(Value, Vec<Value>, Option<String>)> {
    let mut b = Bidi::connect("127.0.0.1", port, o.timeout)?;
    b.set_timeout(o.timeout)?;
    b.new_session()?;
    let ctx = b
        .contexts()?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("BiDi reported no browsing context to navigate"))?;
    b.navigate(&ctx, &o.url, "complete")?;
    let nav = b.evaluate(&ctx, &iife(marionette::NAV_TIMING_SCRIPT))?;
    let res = b.evaluate(&ctx, &iife(marionette::RESOURCE_TIMING_SCRIPT))?;
    let shot = if o.screenshot {
        Some(b.screenshot(&ctx, o.full_page)?)
    } else {
        None
    };
    b.end_session().ok();
    Ok((nav, res.as_array().cloned().unwrap_or_default(), shot))
}

fn drive_rdp(o: &Opts, port: u16) -> Result<(Value, Vec<Value>, Option<String>)> {
    let addr = format!("127.0.0.1:{port}");
    let mut r = Rdp::connect(&addr, o.timeout)?;
    r.set_timeout(o.timeout)?;
    let (_tab, console) = r.active_console()?;

    // No version-stable navigate command, so drive the page itself and wait
    // for it to say it is done.
    r.eval(&console, &format!("location.href = {:?}", o.url))?;
    let deadline = Instant::now() + o.timeout;
    loop {
        if Instant::now() > deadline {
            bail!(
                "the page never reached readyState=complete within {:?}",
                o.timeout
            );
        }
        std::thread::sleep(Duration::from_millis(200));
        // A navigation replaces the console actor's global; a transient error
        // here means the load is in flight, not that it failed.
        let ready = match r.eval(&console, "document.readyState") {
            Ok(v) => v,
            Err(_) => continue,
        };
        let s = ready
            .get("value")
            .and_then(Value::as_str)
            .or_else(|| ready.as_str())
            .unwrap_or_default()
            .to_string();
        if s == "complete" {
            break;
        }
    }

    let nav = rdp_json(&mut r, &console, marionette::NAV_TIMING_SCRIPT)?;
    let res = rdp_json(&mut r, &console, marionette::RESOURCE_TIMING_SCRIPT)?;
    Ok((
        nav,
        res.as_array().cloned().unwrap_or_default(),
        // The console actor has no screenshot command. Parity work uses
        // Marionette or BiDi; RDP's job here is reaching a live session.
        None,
    ))
}

/// RDP's console returns its own value grips rather than plain JSON, so the
/// page is asked to stringify and the string is parsed here. One conversion,
/// in one place, instead of a grip decoder per shape.
fn rdp_json(r: &mut Rdp, console: &str, script: &str) -> Result<Value> {
    let v = r.eval(console, &format!("JSON.stringify({})", iife(script)))?;
    let s = v
        .get("value")
        .and_then(Value::as_str)
        .or_else(|| v.as_str())
        .ok_or_else(|| anyhow!("console returned no string: {v}"))?;
    serde_json::from_str(s).with_context(|| format!("parsing {s:.200}"))
}

/// Wrap a `return`-style script body into an expression. The scripts are
/// written for Marionette's function-body form, and this is what makes the
/// identical text usable from BiDi and RDP.
fn iife(body: &str) -> String {
    format!("(() => {{ {body} }})()")
}

// ----------------------------------------------------------------- conversion

fn nav_timing_from(v: &Value) -> NavigationTiming {
    let g = |k: &str| v.get(k).and_then(Value::as_i64);
    NavigationTiming {
        us_ttfb: g("us_ttfb"),
        us_response_end: g("us_response_end"),
        us_dom_interactive: g("us_dom_interactive"),
        us_dcl: g("us_dcl"),
        us_load: g("us_load"),
        transfer_size: g("transfer_size"),
        encoded_body_size: g("encoded_body_size"),
        decoded_body_size: g("decoded_body_size"),
        dom_nodes: g("dom_nodes"),
    }
}

fn resource_from(v: &Value) -> ResourceTiming {
    let g = |k: &str| v.get(k).and_then(Value::as_i64);
    ResourceTiming {
        url: v
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        initiator: v
            .get("initiator")
            .and_then(Value::as_str)
            .map(str::to_string),
        us_start: g("us_start"),
        us_ttfb: g("us_ttfb"),
        us_end: g("us_end"),
        transfer_size: g("transfer_size"),
        encoded_body_size: g("encoded_body_size"),
        decoded_body_size: g("decoded_body_size"),
    }
}

fn read_version(binary: &PathBuf) -> Option<String> {
    let out = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub fn decode_b64(s: &str) -> Result<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            other => bail!("invalid base64 byte {other:#x}"),
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// Width and height from a PNG's IHDR, so a stored render carries its real
/// pixel size instead of whatever was requested.
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base64_decodes_what_the_ws_encoder_produces() {
        for case in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            let enc = lwdbg_transports::ws::b64(case.as_bytes());
            assert_eq!(decode_b64(&enc).unwrap(), case.as_bytes(), "case {case:?}");
        }
    }

    #[test]
    fn base64_ignores_wrapping_whitespace_but_rejects_garbage() {
        assert_eq!(decode_b64("Zm9v\nYmFy").unwrap(), b"foobar");
        assert!(decode_b64("Zm9v!").is_err());
    }

    #[test]
    fn png_dimensions_read_the_ihdr() {
        let mut p = Vec::new();
        p.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        p.extend_from_slice(&13u32.to_be_bytes());
        p.extend_from_slice(b"IHDR");
        p.extend_from_slice(&1280u32.to_be_bytes());
        p.extend_from_slice(&2048u32.to_be_bytes());
        assert_eq!(png_dimensions(&p), Some((1280, 2048)));
    }

    #[test]
    fn a_non_png_is_none_not_a_panic() {
        assert_eq!(png_dimensions(b"not a png at all......."), None);
        assert_eq!(png_dimensions(b""), None);
    }

    #[test]
    fn the_shared_scripts_survive_being_wrapped_for_bidi_and_rdp() {
        // Marionette gets the bare body; the other two get it as an
        // expression. If this wrapping were wrong, those two transports would
        // silently measure nothing.
        let w = iife(marionette::NAV_TIMING_SCRIPT);
        assert!(w.starts_with("(() => {"));
        assert!(w.trim_end().ends_with("})()"));
        assert!(w.contains("performance.getEntriesByType('navigation')"));
    }

    #[test]
    fn nav_timing_missing_fields_stay_none_rather_than_zero() {
        let nt = nav_timing_from(&json!({"us_ttfb": 221000}));
        assert_eq!(nt.us_ttfb, Some(221000));
        assert_eq!(
            nt.us_load, None,
            "a missing milestone is unknown, not instant"
        );
    }

    #[test]
    fn resource_rows_keep_both_body_sizes() {
        let r = resource_from(&json!({
            "url": "https://neptunium.gd/index.html",
            "initiator": "navigation",
            "encoded_body_size": 1956136,
            "decoded_body_size": 3076779
        }));
        assert_eq!(r.url, "https://neptunium.gd/index.html");
        assert_eq!(r.encoded_body_size, Some(1956136));
        assert_eq!(r.decoded_body_size, Some(3076779));
    }
}
