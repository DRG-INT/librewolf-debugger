//! Launch a Gecko browser with debug transports enabled, in a profile this
//! tool created.
//!
//! Marionette and WebDriver BiDi are **startup-only**: there is no way to turn
//! them on in a running process. That is the whole reason this module exists,
//! and the reason the extension layer exists for the other case.
//!
//! The profile is always a fresh directory under the store. Two prefs make
//! that non-negotiable:
//!
//! * `devtools.debugger.prompt-connection = false` removes the human
//!   confirmation for incoming debugger connections. Writing it into a profile
//!   someone browses with daily would silently remove that protection from
//!   their real session.
//! * `network.proxy.*` points the browser at the local MITM proxy.
//!
//! Neither belongs in anybody's everyday profile, so this module refuses to
//! write into one it did not create.

use anyhow::{anyhow, bail, Context, Result};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Marker file proving the profile is ours to rewrite.
const MARKER: &str = ".lwdbg-scratch-profile";

#[derive(Debug, Clone, Default)]
pub struct ProxyPrefs {
    pub host: String,
    pub port: u16,
    /// Gecko bypasses the proxy for localhost unless this is set, which makes
    /// a local test server invisible to the capture.
    pub include_localhost: bool,
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// Path to the executable, not the .app bundle.
    pub binary: PathBuf,
    pub profile_dir: PathBuf,
    pub headless: bool,
    pub marionette_port: Option<u16>,
    pub rdp_port: Option<u16>,
    pub bidi_port: Option<u16>,
    pub proxy: Option<ProxyPrefs>,
    /// `MOZ_LOG` specification, e.g. `nsHttp:5,timestamp`.
    pub moz_log: Option<String>,
    /// Where the browser's stderr goes. `MOZ_LOG` writes there by default.
    pub log_file: Option<PathBuf>,
    pub extra_args: Vec<String>,
    pub url: Option<String>,
}

pub struct Running {
    pub child: Child,
    pub spec: LaunchSpec,
}

impl Running {
    /// Ask the browser to exit, then kill it if it will not. A Gecko process
    /// left behind holds the profile lock and the next launch fails.
    pub fn shutdown(&mut self, grace: Duration) -> Result<()> {
        #[cfg(unix)]
        // Audited; cargo geiger reports 0/0 unsafe for every crate in this
        // workspace except this one kill(2) call.
        //
        // SAFETY: `kill(2)` takes two integers and touches no memory the
        // caller owns, so there is no pointer, lifetime or aliasing
        // obligation to uphold. The pid comes from our own `Child`, so it is
        // a process this program started. The worst outcome of a race where
        // that child has already exited and the pid was reused is that the
        // signal reaches an unrelated process; the kernel keeps that within
        // what this user owns, and the alternative (`Child::kill`) sends
        // SIGKILL, which would leave Gecko's profile lock behind and break
        // the next launch.
        //
        // The suppression has to be the last line of this block: semgrep
        // honours `nosemgrep` on the finding's line or the one immediately
        // above it, and nowhere else. The SAFETY text one line further up is
        // still in the same contiguous comment, which is what matters.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            libc_kill(self.child.id() as i32, 15);
        }
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if let Some(_s) = self.child.try_wait()? {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.child.kill().ok();
        self.child.wait().ok();
        Ok(())
    }
}

#[cfg(unix)]
// Audited; the SAFETY comment states the obligations. cargo
// geiger reports 0/0 unsafe for every crate here except this
// one kill(2) call.
// nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
unsafe fn libc_kill(pid: i32, sig: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, sig);
}

/// Resolve a macOS `.app` bundle, or a plain path, to the executable inside.
pub fn resolve_binary(path: &Path) -> Result<PathBuf> {
    let p = std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    if p.extension().and_then(|e| e.to_str()) == Some("app") {
        let macos = p.join("Contents/MacOS");
        let mut best: Option<PathBuf> = None;
        for e in std::fs::read_dir(&macos)
            .with_context(|| format!("{} is a bundle with no Contents/MacOS", p.display()))?
        {
            let e = e?;
            if e.file_type()?.is_file() {
                let name = e.file_name().to_string_lossy().to_lowercase();
                // The launcher shares the bundle name; helpers do not.
                if !name.contains("helper") && !name.contains("crash") {
                    best = Some(e.path());
                    if p.file_stem()
                        .map(|s| s.to_string_lossy().to_lowercase())
                        .as_deref()
                        == Some(name.as_str())
                    {
                        break;
                    }
                }
            }
        }
        return best.ok_or_else(|| anyhow!("no executable found in {}", macos.display()));
    }
    if !p.is_file() {
        bail!("{} is not a file", p.display());
    }
    Ok(p)
}

/// Create (or refresh) a scratch profile. Refuses any directory that exists
/// without our marker — that could be somebody's real profile.
pub fn prepare_profile(dir: &Path, spec: &LaunchSpec) -> Result<()> {
    if dir.exists() {
        let is_ours = dir.join(MARKER).exists();
        let is_empty = std::fs::read_dir(dir)?.next().is_none();
        if !is_ours && !is_empty {
            bail!(
                "{} already exists and was not created by lwdbg. This command \
                 writes devtools.debugger.prompt-connection=false, which must \
                 never be written into a profile you browse with. Choose an \
                 empty directory.",
                dir.display()
            );
        }
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join(MARKER),
        "Created by lwdbg. Safe to delete. Debug prefs live in user.js.\n",
    )?;
    std::fs::write(dir.join("user.js"), user_js(spec))?;
    Ok(())
}

pub fn user_js(spec: &LaunchSpec) -> String {
    let b = |k: &str, v: bool| format!("user_pref({k:?}, {v});\n");
    let i = |k: &str, v: u16| format!("user_pref({k:?}, {v});\n");
    let s = |k: &str, v: &str| format!("user_pref({k:?}, {v:?});\n");

    let mut out =
        String::from("// Generated by lwdbg. Debug-only profile — do not reuse for browsing.\n");

    // Remote debugging. force-local keeps the listener on loopback: a debug
    // port reachable from the network is a remote code execution surface.
    out += &b("devtools.debugger.remote-enabled", true);
    out += &b("devtools.debugger.force-local", true);
    out += &b("devtools.chrome.enabled", true);
    out += &b("devtools.debugger.prompt-connection", false);
    if let Some(p) = spec.rdp_port {
        out += &i("devtools.debugger.remote-port", p);
    }
    if let Some(p) = spec.marionette_port {
        out += &i("marionette.port", p);
    }

    // Keep the run reproducible: no update checks, no telemetry pings, no
    // session restore, no first-run page. Each of those is extra network
    // traffic that would land in the capture and skew the timing.
    out += &b("app.update.auto", false);
    out += &b("app.update.enabled", false);
    out += &b("browser.shell.checkDefaultBrowser", false);
    // These three are string/bool prefs that already have a declared type.
    // Writing the wrong type into user.js is not ignored: Gecko stops
    // applying the file, and the profile comes up without Marionette at all.
    // Verified against LibreWolf 153.0.1 — three mistyped prefs were enough
    // to make every launch fail with no error message naming them.
    out += &s("browser.startup.homepage_override.mstone", "ignore");
    out += &s("browser.startup.homepage", "about:blank");
    out += &s("startup.homepage_welcome_url", "");
    out += &s("startup.homepage_welcome_url.additional", "");
    out += &b("browser.sessionstore.resume_from_crash", false);
    out += &b("datareporting.healthreport.uploadEnabled", false);
    out += &b("datareporting.policy.dataSubmissionEnabled", false);
    out += &b("toolkit.telemetry.enabled", false);
    out += &b("toolkit.telemetry.unified", false);
    out += &b("extensions.getAddons.cache.enabled", false);
    out += &b("browser.safebrowsing.malware.enabled", false);
    out += &b("browser.safebrowsing.phishing.enabled", false);
    out += &b("network.captive-portal-service.enabled", false);
    out += &b("network.connectivity-service.enabled", false);
    out += &s("browser.region.network.url", "");
    out += &b("browser.newtabpage.enabled", false);
    out += &b(
        "browser.newtabpage.activity-stream.feeds.system.topstories",
        false,
    );

    // Fingerprinting resistance destroys the Performance APIs, which are the
    // whole measurement. LibreWolf turns it on for everyone, and with it on
    // this build reported a *negative* responseStart, a navigationStart of 0,
    // and zero resource timing entries — every number the tool exists to
    // collect was either missing or nonsense.
    //
    // Two reasons this is safe to write here and nowhere else:
    //  * the profile is scratch, created by this tool, never browsed with;
    //  * LibreWolf sets it with `defaultPref()`, so a user pref genuinely
    //    overrides it. (`devtools.debugger.remote-enabled` is set with plain
    //    `pref()` in the same file, which runs *after* user.js and therefore
    //    cannot be overridden this way — that is why RDP is unavailable on
    //    LibreWolf while Marionette and BiDi work.)
    out += &b("privacy.resistFingerprinting", false);
    out += &b("privacy.resistFingerprinting.pbmode", false);
    out += &b("privacy.reduceTimerPrecision", false);
    out += &b("privacy.trackingprotection.enabled", false);

    // Measurement hygiene: the browser's own HTTP cache would make the second
    // run of a page meaningless. The proxy cache is the one under study, and
    // it is controlled separately.
    out += &b("browser.cache.disk.enable", false);
    out += &b("browser.cache.memory.enable", false);

    if let Some(p) = &spec.proxy {
        out += &i("network.proxy.type", 1);
        out += &s("network.proxy.http", &p.host);
        out += &i("network.proxy.http_port", p.port);
        out += &s("network.proxy.ssl", &p.host);
        out += &i("network.proxy.ssl_port", p.port);
        out += &b("network.proxy.share_proxy_settings", true);
        out += &s("network.proxy.no_proxies_on", "");
        if p.include_localhost {
            out += &b("network.proxy.allow_hijacking_localhost", true);
        }
        // The proxy mints leaf certificates from a local CA. Gecko keeps a
        // per-profile trust store, so the CA still has to be added to this
        // profile's cert9.db; this pref only lets the platform store count too.
        out += &b("security.enterprise_roots.enabled", true);
    }

    out
}

pub fn args(spec: &LaunchSpec) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--profile".into(),
        spec.profile_dir.display().to_string(),
        "--no-remote".into(),
        "--new-instance".into(),
    ];
    if spec.headless {
        a.push("--headless".into());
    }
    if spec.marionette_port.is_some() {
        a.push("--marionette".into());
    }
    if let Some(p) = spec.rdp_port {
        // The flag takes the port; the pref alone does not start the listener.
        a.push("--start-debugger-server".into());
        a.push(p.to_string());
    }
    if let Some(p) = spec.bidi_port {
        a.push("--remote-debugging-port".into());
        a.push(p.to_string());
    }
    a.extend(spec.extra_args.iter().cloned());
    if let Some(u) = &spec.url {
        a.push(u.clone());
    }
    a
}

pub fn spawn(spec: LaunchSpec) -> Result<Running> {
    prepare_profile(&spec.profile_dir.clone(), &spec)?;

    let mut cmd = Command::new(&spec.binary);
    cmd.args(args(&spec));

    // A fresh profile plus an explicit port is still not enough on its own:
    // the environment variable is what geckodriver uses and what some builds
    // honour over the pref.
    if let Some(p) = spec.marionette_port {
        cmd.env("MOZ_MARIONETTE_PORT", p.to_string());
    }
    if let Some(l) = &spec.moz_log {
        cmd.env("MOZ_LOG", l);
        if let Some(f) = &spec.log_file {
            cmd.env("MOZ_LOG_FILE", f);
        }
    }
    // Keep crash reporting from phoning home mid-measurement.
    cmd.env("MOZ_CRASHREPORTER_DISABLE", "1");
    cmd.env("MOZ_DISABLE_AUTO_SAFE_MODE", "1");

    match &spec.log_file {
        Some(f) => {
            let out =
                std::fs::File::create(f).with_context(|| format!("creating {}", f.display()))?;
            let err = out.try_clone()?;
            cmd.stdout(Stdio::from(out)).stderr(Stdio::from(err));
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", spec.binary.display()))?;
    Ok(Running { child, spec })
}

/// Poll a loopback port until something accepts, or give up.
///
/// Gecko binds Marionette late in startup — several seconds on a cold profile
/// — so a single immediate connect attempt reliably fails on a correct setup.
pub fn wait_for_port(port: u16, timeout: Duration) -> Result<()> {
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    bail!(
        "port {port} never accepted a connection within {:?} (last error: {last}). \
         The browser may have exited, or the build may not support the flag.",
        timeout
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(dir: &Path) -> LaunchSpec {
        LaunchSpec {
            binary: PathBuf::from("/bin/true"),
            profile_dir: dir.to_path_buf(),
            headless: true,
            marionette_port: Some(2828),
            rdp_port: Some(6000),
            bidi_port: Some(9222),
            proxy: Some(ProxyPrefs {
                host: "127.0.0.1".into(),
                port: 8899,
                include_localhost: true,
            }),
            moz_log: Some("nsHttp:5,timestamp".into()),
            log_file: None,
            extra_args: vec![],
            url: Some("https://example.com/".into()),
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d =
            // Test helper. The name carries the pid and the directory is
            // removed first; nothing secret is written here, and a test that
            // could not use a temp dir would need a fixture in the repo.
            // nosemgrep: rust.lang.security.temp-dir.temp-dir
            std::env::temp_dir().join(format!("lwdbg-launch-test-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        d
    }

    #[test]
    fn prompt_connection_is_disabled_only_inside_a_generated_profile() {
        let js = user_js(&spec(Path::new("/tmp/x")));
        assert!(js.contains("\"devtools.debugger.prompt-connection\", false"));
        assert!(js.contains("\"devtools.debugger.force-local\", true"));
    }

    #[test]
    fn refuses_a_directory_it_did_not_create() {
        let d = tmp("foreign");
        std::fs::create_dir_all(&d).unwrap();
        // Looks like a real profile.
        std::fs::write(d.join("cert9.db"), b"not really").unwrap();
        std::fs::write(d.join("logins.json"), b"{}").unwrap();
        let e = prepare_profile(&d, &spec(&d)).unwrap_err().to_string();
        assert!(e.contains("not created by lwdbg"), "got: {e}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn accepts_an_empty_directory_and_then_its_own() {
        let d = tmp("empty");
        std::fs::create_dir_all(&d).unwrap();
        prepare_profile(&d, &spec(&d)).unwrap();
        assert!(d.join("user.js").exists());
        assert!(d.join(MARKER).exists());
        // Second run must succeed: re-measuring is the normal case.
        prepare_profile(&d, &spec(&d)).unwrap();
        std::fs::remove_dir_all(&d).ok();
    }

    /// Every pref whose declared type is known, with the type it must be
    /// written as. A mismatch here is not cosmetic: Gecko abandons the rest
    /// of user.js, and the observed symptom is a launch where no transport
    /// ever starts and nothing in the log says why.
    #[test]
    fn no_pref_is_written_with_the_wrong_type() {
        let js = user_js(&spec(Path::new("/tmp/x")));
        let want_string = [
            "browser.startup.homepage",
            "browser.startup.homepage_override.mstone",
            "startup.homepage_welcome_url",
            "startup.homepage_welcome_url.additional",
            "browser.region.network.url",
            "network.proxy.http",
            "network.proxy.ssl",
            "network.proxy.no_proxies_on",
        ];
        let want_bool = [
            "devtools.debugger.remote-enabled",
            "devtools.chrome.enabled",
            "devtools.debugger.prompt-connection",
            "browser.cache.disk.enable",
            "browser.cache.memory.enable",
            "browser.newtabpage.enabled",
            "browser.newtabpage.activity-stream.feeds.system.topstories",
            "toolkit.telemetry.enabled",
        ];
        let want_int = [
            "network.proxy.type",
            "network.proxy.http_port",
            "marionette.port",
        ];

        for line in js.lines().filter(|l| l.starts_with("user_pref(")) {
            let name = line
                .split('"')
                .nth(1)
                .unwrap_or_else(|| panic!("unparsable line: {line}"));
            let value = line
                .rsplit_once(", ")
                .map(|(_, v)| v.trim_end_matches(");"))
                .unwrap_or("");
            if want_string.contains(&name) {
                assert!(
                    value.starts_with('"'),
                    "{name} is a string pref but was written as {value}"
                );
            } else if want_bool.contains(&name) {
                assert!(
                    value == "true" || value == "false",
                    "{name} is a bool pref but was written as {value}"
                );
            } else if want_int.contains(&name) {
                assert!(
                    value.parse::<i64>().is_ok(),
                    "{name} is an int pref but was written as {value}"
                );
            }
        }
    }

    /// Without this the Performance APIs return nonsense on LibreWolf, and
    /// the failure is silent: numbers come back, they are just wrong.
    #[test]
    fn fingerprinting_resistance_is_off_in_a_measurement_profile() {
        let js = user_js(&spec(Path::new("/tmp/x")));
        assert!(js.contains("\"privacy.resistFingerprinting\", false"));
        assert!(js.contains("\"privacy.reduceTimerPrecision\", false"));
    }

    #[test]
    fn the_browser_cache_is_off_so_a_second_run_still_measures_the_network() {
        let js = user_js(&spec(Path::new("/tmp/x")));
        assert!(js.contains("\"browser.cache.disk.enable\", false"));
        assert!(js.contains("\"browser.cache.memory.enable\", false"));
    }

    #[test]
    fn proxy_prefs_cover_both_schemes() {
        let js = user_js(&spec(Path::new("/tmp/x")));
        assert!(js.contains("\"network.proxy.http_port\", 8899"));
        assert!(js.contains("\"network.proxy.ssl_port\", 8899"));
        assert!(js.contains("\"network.proxy.allow_hijacking_localhost\", true"));
    }

    #[test]
    fn no_proxy_means_no_proxy_prefs_at_all() {
        let mut s = spec(Path::new("/tmp/x"));
        s.proxy = None;
        let js = user_js(&s);
        assert!(!js.contains("network.proxy.type"));
    }

    #[test]
    fn argv_order_puts_the_profile_first_and_the_url_last() {
        let a = args(&spec(Path::new("/tmp/p")));
        assert_eq!(a[0], "--profile");
        assert_eq!(a[1], "/tmp/p");
        assert_eq!(a.last().unwrap(), "https://example.com/");
        assert!(a.contains(&"--headless".to_string()));
        assert!(a.contains(&"--marionette".to_string()));
        let i = a
            .iter()
            .position(|x| x == "--start-debugger-server")
            .unwrap();
        assert_eq!(
            a[i + 1],
            "6000",
            "the flag carries the port, not just the pref"
        );
        let j = a
            .iter()
            .position(|x| x == "--remote-debugging-port")
            .unwrap();
        assert_eq!(a[j + 1], "9222");
    }

    #[test]
    fn a_port_nobody_listens_on_times_out_with_a_useful_message() {
        // Port 1 is privileged and unbound; the point is the message shape.
        let e = wait_for_port(1, Duration::from_millis(400))
            .unwrap_err()
            .to_string();
        assert!(e.contains("never accepted"), "got: {e}");
        assert!(e.contains("last error"), "got: {e}");
    }
}
