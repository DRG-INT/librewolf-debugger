//! `lwdbg` — measure, debug and control browsers.
//!
//! The proxy is the backbone because it is the only layer that reaches every
//! engine, including Goanna where no extension can be installed. The SSoT is
//! the canonical store: every signal lands there under one trace id so the
//! proxy's view, the browser's timing API and the logs can be joined.

mod adapters;
mod android;
mod browser;
mod detect;
mod doctor;
mod measure;
mod vantage;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use lwdbg_proxy::server::{self, CacheMode, Config};
use lwdbg_ssot::Store;
use lwdbg_transports::Transport;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "lwdbg",
    about = "Measure, debug and control browsers through a caching proxy and a correlated debug store",
    long_about = "The proxy reaches every engine (Gecko, Blink, WebKit, Goanna). Gecko forks \
                  additionally support Marionette/RDP/BiDi and extension control.\n\n\
                  Measurements default to 3 runs and report the median with the spread: a \
                  single observation is not a property of a server.",
    version
)]
struct Cli {
    /// Store directory (SSoT database, blobs, CA). Defaults to
    /// $LWDBG_HOME, else ~/.local/share/lwdbg
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    /// Machine-readable output. Use this whenever another program or agent
    /// consumes the result.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// What works on this machine, and what is missing. Run this first.
    Doctor,
    /// What this *build* can do, as opposed to what this machine has.
    /// Machine-readable; read this instead of inferring from a flag.
    Capabilities,
    /// Installed browsers, their engines, and which transports they can use.
    Detect,
    /// Run the caching proxy.
    Proxy(ProxyArgs),
    /// Measure a URL. Three runs and a median by default.
    Measure(MeasureArgs),
    /// Query the debug store.
    Ssot(SsotArgs),
    /// Wire this tool into the agent harnesses on this machine.
    Adapters(AdaptersArgs),
    /// Connect to a debug transport that is already listening and report what
    /// is there. This is the route to a browser that has been open for days.
    Attach(AttachArgs),
    /// Start a browser with debug transports enabled, in a scratch profile.
    Launch(LaunchArgs),
    /// MOZ_LOG: what to collect, and how to fold it into the store.
    Logs(LogsArgs),
    /// Evaluate JavaScript in a browser reached over a debug transport.
    /// Pair it with `launch` to drive a session by hand.
    Eval(EvalArgs),
    /// Compare how browsers render the same URL. Scores are evidence for a
    /// human, not a verdict: engines differ by design.
    Parity(ParityArgs),
    /// Act on a browser that is **already running**, through the extension
    /// bridge. The only route to a session that must not be restarted.
    Live(LiveArgs),
    /// Android Firefox over adb: debugging, real multitouch input, screen
    /// capture. The RDP client from the desktop path serves this unchanged.
    Android(AndroidArgs),
    /// Measure the same target from other machines. Two or more vantage
    /// points are what separates a slow origin from a slow route.
    Vantage(VantageArgs),
}

#[derive(Args)]
struct AdaptersArgs {
    #[command(subcommand)]
    cmd: AdaptersCmd,
}

#[derive(Subcommand)]
enum AdaptersCmd {
    /// Every known harness: installed here, where the entry point goes, and
    /// whether it is already wired.
    List,
    /// Generate entry points. One canonical source, many native formats.
    Install {
        /// Harness id (repeatable). Omit with --all for every harness found.
        #[arg(long)]
        harness: Vec<String>,
        /// Every harness detected on this machine.
        #[arg(long)]
        all: bool,
        /// Write to this path instead of the harness default. Required for
        /// the generic `agents` harness.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Copy the skill directory instead of symlinking it. A copy goes
        /// stale; prefer the symlink unless the harness cannot follow one.
        #[arg(long)]
        copy: bool,
        /// Replace a file this tool did not generate.
        #[arg(long)]
        force: bool,
        /// Report what would happen and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print a harness's entry point to stdout without installing it. Use
    /// this for any CLI not in the table.
    Emit {
        #[arg(long)]
        harness: String,
    },
    /// Remove generated entry points.
    Uninstall {
        #[arg(long)]
        harness: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Args)]
struct ProxyArgs {
    #[command(subcommand)]
    cmd: ProxyCmd,
}

#[derive(Subcommand)]
enum ProxyCmd {
    /// Start the proxy and run until interrupted.
    Start {
        #[arg(long, default_value = "127.0.0.1:8899")]
        listen: String,
        /// Intercept only these hosts (repeatable). Subdomains are included.
        /// Omitting this intercepts everything, which is a wide grant — a
        /// trusted-CA proxy can read credentials and session cookies.
        #[arg(long = "only-host")]
        only_host: Vec<String>,
        /// live: consult and revalidate. cached: serve any hit, fresh or not
        /// (deterministic, and not representative of a real visitor).
        /// bypass: never touch the cache.
        #[arg(long, default_value = "live")]
        cache_mode: String,
        /// Intercept Tor Browser traffic. Refused by default: a local MITM
        /// breaks its threat model.
        #[arg(long)]
        force_tor: bool,
    },
    /// Print the CA certificate in PEM form.
    CaExport,
    /// Install the CA into a Gecko profile's cert9.db via certutil.
    CaInstall {
        /// Path to the profile directory holding cert9.db.
        #[arg(long)]
        profile: PathBuf,
    },
    /// Remove the CA from a Gecko profile's cert9.db.
    CaUninstall {
        #[arg(long)]
        profile: PathBuf,
    },
}

#[derive(Args)]
struct MeasureArgs {
    url: String,
    /// Number of runs. Lowering this to 1 makes the result unreportable.
    #[arg(long, default_value_t = 3)]
    runs: usize,
    /// How to measure. `curl` is the baseline every other number is checked
    /// against. `marionette` and `bidi` drive a fresh browser; `rdp` also
    /// works against a browser that is already running.
    #[arg(long, default_value = "curl")]
    via: String,
    /// Browser binary or .app bundle. Required for any non-curl `--via`.
    #[arg(long)]
    browser: Option<PathBuf>,
    /// Run the browser with a visible window. Headless is the default.
    #[arg(long)]
    headed: bool,
    /// Send the browser's traffic through a proxy, as host:port.
    #[arg(long)]
    proxy: Option<String>,
    /// Capture a screenshot into the store for each run.
    #[arg(long)]
    screenshot: bool,
    /// Capture the whole page rather than the viewport.
    #[arg(long)]
    full_page: bool,
    /// MOZ_LOG specification. Pass `default` for the curated set.
    #[arg(long)]
    moz_log: Option<String>,
    /// Keep the scratch profile for inspection instead of deleting it.
    #[arg(long)]
    keep_profile: bool,
    /// Seconds to allow for the transport and for each navigation.
    #[arg(long, default_value_t = 120)]
    timeout: u64,
    /// Free-text label stored with the trace.
    #[arg(long)]
    label: Option<String>,
    /// Request identity encoding instead of offering compression (curl only).
    #[arg(long)]
    no_compress: bool,
}

#[derive(Args)]
struct AttachArgs {
    /// marionette | rdp | bidi. Only `rdp` can reach a browser that is
    /// already running; the other two are startup-only.
    #[arg(long, default_value = "rdp")]
    transport: String,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Defaults to the transport's documented port.
    #[arg(long)]
    port: Option<u16>,
    #[arg(long, default_value_t = 15)]
    timeout: u64,
}

#[derive(Args)]
struct LaunchArgs {
    /// Browser binary or .app bundle.
    #[arg(long)]
    browser: PathBuf,
    /// Transports to enable (repeatable): marionette, rdp, bidi.
    #[arg(long = "transport", default_values_t = [String::from("rdp")])]
    transports: Vec<String>,
    /// Profile directory. Must be empty or previously created by lwdbg.
    #[arg(long)]
    profile: Option<PathBuf>,
    #[arg(long)]
    headed: bool,
    #[arg(long)]
    url: Option<String>,
    /// host:port of a proxy to route through.
    #[arg(long)]
    proxy: Option<String>,
    /// MOZ_LOG specification; `default` for the curated set.
    #[arg(long)]
    moz_log: Option<String>,
    /// Print the command line and exit without starting anything. Use this
    /// when you want to run the browser yourself.
    #[arg(long)]
    print_only: bool,
}

#[derive(Args)]
struct AndroidArgs {
    /// Device serial, when more than one is attached.
    #[arg(long, global = true)]
    serial: Option<String>,
    #[command(subcommand)]
    cmd: AndroidCmd,
}

#[derive(Subcommand)]
enum AndroidCmd {
    /// Attached devices and whether each is usable yet.
    Devices,
    /// Firefox debugger sockets found on the device. Read from the device
    /// rather than guessed: the name is build- and package-dependent.
    Sockets,
    /// Forward a device socket to a local port and report what RDP says.
    Attach {
        /// Abstract socket name. Omitted means the first one found.
        #[arg(long)]
        socket: Option<String>,
        #[arg(long, default_value_t = 6080)]
        port: u16,
        /// Leave the forward in place after reporting, so other commands can
        /// use it.
        #[arg(long)]
        keep: bool,
    },
    /// Screen size in pixels.
    Size,
    /// Tap. Real device input, which is what multitouch actually means here.
    Tap { x: i32, y: i32 },
    /// Swipe, in milliseconds.
    Swipe {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[arg(default_value_t = 300)]
        ms: u32,
    },
    /// Type text. Quotes and shell metacharacters are refused.
    Text { text: String },
    /// Key event, e.g. BACK, HOME, ENTER, 66.
    Key { key: String },
    /// Capture the screen into the store.
    Screenshot,
    /// Point the device's browser at a proxy running on this machine.
    Proxy {
        /// Port the proxy listens on here.
        #[arg(long, default_value_t = 8899)]
        host_port: u16,
        /// Port the device should use. Usually the same number.
        #[arg(long)]
        device_port: Option<u16>,
    },
}

#[derive(Args)]
struct VantageArgs {
    url: String,
    /// Host to measure from (repeatable). `local` is valid and is measured
    /// the same way as the others.
    #[arg(long = "host", default_values_t = [String::from("local")])]
    hosts: Vec<String>,
    #[arg(long, default_value_t = 3)]
    runs: usize,
    #[arg(long, default_value_t = 30)]
    timeout: u64,
}

#[derive(Args)]
struct LiveArgs {
    #[command(subcommand)]
    cmd: LiveCmd,
}

#[derive(Subcommand)]
enum LiveCmd {
    /// Install the extension and the native messaging host manifests.
    Install {
        /// Also print the manual load-temporary-add-on steps.
        #[arg(long)]
        explain: bool,
    },
    /// Remove the native messaging manifests this tool wrote.
    Uninstall,
    /// What the connected extension can do.
    Commands,
    /// Tabs open in the running browser.
    Tabs,
    /// Windows open in the running browser.
    Windows,
    /// Page timing from a tab the user already has open.
    Timing {
        #[arg(long)]
        tab: Option<i64>,
    },
    /// Screenshot the visible area of a tab, into the store.
    Screenshot {
        #[arg(long)]
        tab: Option<i64>,
    },
    /// Any command, with explicit parameters. Use `commands` to see the list.
    Call {
        command: String,
        /// JSON object of parameters.
        #[arg(long, default_value = "{}")]
        params: String,
        /// Permit a command that changes the live session. Required for
        /// anything that navigates, opens, closes or evaluates. Ask the user
        /// before each one — approval does not carry over.
        #[arg(long)]
        allow_write: bool,
    },
}

#[derive(Args)]
struct ParityArgs {
    /// URL to render in each browser. Omit when comparing stored renders.
    #[arg(long)]
    url: Option<String>,
    /// Browser binary or .app bundle (repeatable). Two or more to compare.
    #[arg(long = "browser")]
    browsers: Vec<PathBuf>,
    /// Compare two renders already in the store instead of rendering.
    #[arg(long)]
    render_a: Option<i64>,
    #[arg(long)]
    render_b: Option<i64>,
    /// auto | gpu | cpu. `auto` uses the GPU when there is one and says which
    /// it used.
    #[arg(long, default_value = "auto")]
    backend: String,
    #[arg(long)]
    full_page: bool,
    #[arg(long, default_value = "marionette")]
    via: String,
    #[arg(long, default_value_t = 180)]
    timeout: u64,
}

#[derive(Args)]
struct EvalArgs {
    /// The script. Marionette and RDP take a function body (use `return`);
    /// BiDi takes an expression. `--expr` wraps a body for all three.
    script: String,
    #[arg(long, default_value = "marionette")]
    transport: String,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long)]
    port: Option<u16>,
    /// Navigate here first, and wait for the load.
    #[arg(long)]
    url: Option<String>,
    #[arg(long, default_value_t = 60)]
    timeout: u64,
}

#[derive(Args)]
struct LogsArgs {
    #[command(subcommand)]
    cmd: LogsCmd,
}

#[derive(Subcommand)]
enum LogsCmd {
    /// The log modules worth asking for, and what each one answers.
    Modules,
    /// Parse a MOZ_LOG file and store it under a trace.
    Ingest {
        file: PathBuf,
        /// Trace to attach the lines to. Without one, a trace is created.
        #[arg(long)]
        trace: Option<String>,
        /// Only keep lines naming this URL substring.
        #[arg(long)]
        url: Option<String>,
    },
}

#[derive(Args)]
struct SsotArgs {
    #[command(subcommand)]
    cmd: SsotCmd,
}

#[derive(Subcommand)]
enum SsotCmd {
    /// List recent traces.
    Traces {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Exchanges recorded under one trace.
    Trace { id: String },
    /// Whether compression actually applied, per exchange.
    Compression {
        #[arg(long)]
        url: Option<String>,
    },
}

const CA_NICKNAME: &str = "lwdbg local debugging CA";

fn home_dir(cli: &Cli) -> Result<PathBuf> {
    if let Some(h) = &cli.home {
        return Ok(h.clone());
    }
    if let Some(h) = std::env::var_os("LWDBG_HOME") {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/share/lwdbg"))
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LWDBG_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let home = home_dir(&cli)?;
    let store_dir = home.join("store");
    let ca_dir = home.join("ca");

    match &cli.cmd {
        Cmd::Doctor => {
            let checks = doctor::run()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&checks)?);
            } else {
                for c in &checks {
                    println!(
                        "{} {:<28} {}",
                        if c.ok { "ok  " } else { "FAIL" },
                        c.name,
                        c.detail
                    );
                    if let Some(r) = &c.remedy {
                        println!("                                  -> {r}");
                    }
                }
            }
            if checks.iter().any(|c| !c.ok) {
                std::process::exit(1);
            }
            Ok(())
        }

        Cmd::Capabilities => {
            // Separate from `doctor` on purpose. `doctor` answers "what is
            // installed here", which changes when somebody runs brew. This
            // answers "what was compiled in", which changes only when the
            // binary is rebuilt — and a script that confuses the two reports
            // a failure for a configuration choice.
            // Empty when built by a bare `cargo build`; set by `make install`
            // through cli/build.rs. Null rather than "" so a consumer can tell
            // "not installed" from "installed at the empty path".
            let baked = env!("LWDBG_DATADIR");
            let baked_datadir = if baked.is_empty() {
                Value::Null
            } else {
                Value::String(baked.to_string())
            };
            // The GPU path is a separate binary, so this is a question about
            // the filesystem rather than about compile-time features. The lib
            // feature is reported too, because it is what decides whether
            // in-process GPU code exists at all — and in this build it never
            // does, by design.
            let helper = gpu_helper();
            let caps = serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "gpu_backend_linked": lwdbg_gpu::gpu_compiled_in(),
                "gpu_helper": helper.as_ref().map(|p| p.display().to_string()),
                "parity_backends": if helper.is_some() {
                    vec!["gpu", "cpu"]
                } else {
                    vec!["cpu"]
                },
                "transports": ["marionette", "rdp", "bidi"],
                "data_dir_baked_in": baked_datadir,
                "live_bridge": cfg!(unix),
            });
            println!("{}", serde_json::to_string_pretty(&caps)?);
            Ok(())
        }

        Cmd::Detect => {
            let found = detect::detect()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&found)?);
            } else if found.is_empty() {
                println!("no known browsers found");
            } else {
                for f in &found {
                    println!(
                        "{:<24} {:<8} {}",
                        f.name,
                        f.engine,
                        f.version.as_deref().unwrap_or("?")
                    );
                    println!("  binary     {}", f.binary);
                    if let Some(r) = &f.resolved_path {
                        println!("  symlink -> {r}");
                    }
                    println!(
                        "  transport  {}   webextensions {}",
                        if f.has_debug_transport { "yes" } else { "no " },
                        if f.supports_webextensions {
                            "yes"
                        } else {
                            "no"
                        }
                    );
                    if let Some(x) = &f.excluded {
                        println!("  EXCLUDED   {x}");
                    }
                }
            }
            Ok(())
        }

        Cmd::Proxy(p) => match &p.cmd {
            ProxyCmd::Start {
                listen,
                only_host,
                cache_mode,
                force_tor,
            } => {
                let mode = match cache_mode.as_str() {
                    "live" => CacheMode::Live,
                    "cached" => CacheMode::Cached,
                    "bypass" => CacheMode::Bypass,
                    other => return Err(anyhow!("unknown cache mode {other:?}")),
                };
                let cfg = Config {
                    listen: listen
                        .parse()
                        .with_context(|| format!("parsing {listen}"))?,
                    only_hosts: only_host.clone(),
                    cache_mode: mode,
                    store_dir,
                    ca_dir,
                    allow_tor: *force_tor,
                };
                run_proxy(cfg, cli.json)
            }
            ProxyCmd::CaExport => {
                let ca = lwdbg_proxy::ca::Ca::load_or_create(&ca_dir)?;
                print!("{}", ca.ca_cert_pem());
                Ok(())
            }
            ProxyCmd::CaInstall { profile } => {
                let ca = lwdbg_proxy::ca::Ca::load_or_create(&ca_dir)?;
                ca_install(&ca, profile, &ca_dir)?;
                println!(
                    "installed {CA_NICKNAME:?} into {}\n\
                     This profile's browser now trusts the proxy to read all of its TLS \
                     traffic. Remove it with `lwdbg proxy ca-uninstall --profile <path>` when \
                     the work is done.",
                    profile.display()
                );
                Ok(())
            }
            ProxyCmd::CaUninstall { profile } => {
                let out = std::process::Command::new("certutil")
                    .args(["-D", "-n", CA_NICKNAME, "-d"])
                    .arg(format!("sql:{}", profile.display()))
                    .output()
                    .context("running certutil")?;
                if !out.status.success() {
                    return Err(anyhow!(
                        "certutil failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ));
                }
                println!("removed {CA_NICKNAME:?} from {}", profile.display());
                Ok(())
            }
        },

        Cmd::Measure(m) if m.via != "curl" => {
            let mut store = Store::open(&store_dir)?;
            run_browser_measure(&mut store, m, &home, cli.json)
        }

        Cmd::Measure(m) => {
            let store = Store::open(&store_dir)?;
            let s = measure::curl_baseline(&store, &m.url, m.runs, !m.no_compress)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                println!("{} via {}", s.url, s.via);
                for r in &s.runs {
                    println!(
                        "  run{} status={} TTFB={:.3}s total={:.3}s wire={}B",
                        r.run,
                        r.status,
                        r.us_ttfb as f64 / 1e6,
                        r.us_total as f64 / 1e6,
                        r.wire_bytes
                    );
                }
                if let Some(t) = s.median_us_ttfb {
                    println!("  median TTFB  {:.3}s", t / 1e6);
                }
                if let Some(t) = s.median_us_total {
                    println!("  median total {:.3}s", t / 1e6);
                }
                if let (Some(mn), Some(mx)) = (s.min_us_total, s.max_us_total) {
                    println!(
                        "  range        {:.3}s .. {:.3}s",
                        mn as f64 / 1e6,
                        mx as f64 / 1e6
                    );
                }
                if !s.note.is_empty() {
                    println!("  note: {}", s.note);
                }
            }
            Ok(())
        }

        Cmd::Adapters(a) => run_adapters(a, cli.json),

        Cmd::Attach(a) => run_attach(a, cli.json),

        Cmd::Launch(l) => run_launch(l, &home, cli.json),

        Cmd::Logs(l) => run_logs(l, &store_dir, cli.json),

        Cmd::Eval(e) => run_eval(e),

        Cmd::Android(a) => run_android(&store_dir, a, cli.json),

        Cmd::Vantage(v) => {
            let r = vantage::measure(&v.url, &v.hosts, v.runs, v.timeout)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!("{}", r.url);
                for h in &r.hosts {
                    if !h.reachable {
                        println!(
                            "  {:<16} unreachable: {}",
                            h.host,
                            h.error.as_deref().unwrap_or("")
                        );
                        continue;
                    }
                    println!(
                        "  {:<16} {:<18} TTFB {}  total {}",
                        h.host,
                        h.uname.as_deref().unwrap_or(""),
                        h.median_us_ttfb
                            .map(|v| format!("{:.3}s", v / 1e6))
                            .unwrap_or_else(|| "?".into()),
                        h.median_us_total
                            .map(|v| format!("{:.3}s", v / 1e6))
                            .unwrap_or_else(|| "?".into()),
                    );
                    if let Some(e) = &h.error {
                        println!("                   note: {e}");
                    }
                }
                println!("\n  {}", r.reading);
            }
            Ok(())
        }

        Cmd::Live(l) => {
            let mut store = Store::open(&store_dir)?;
            run_live(&mut store, l, &home, cli.json)
        }

        Cmd::Parity(a) => {
            let mut store = Store::open(&store_dir)?;
            run_parity(&mut store, a, &home, cli.json)
        }

        Cmd::Ssot(s) => {
            let store = Store::open(&store_dir)?;
            match &s.cmd {
                SsotCmd::Traces { limit } => {
                    let mut st = store.conn().prepare(
                        "SELECT id, started_at, target_url, via, cache_mode, label
                           FROM trace ORDER BY started_at DESC LIMIT ?1",
                    )?;
                    let rows: Vec<serde_json::Value> = st
                        .query_map(rusqlite::params![*limit as i64], |r| {
                            Ok(serde_json::json!({
                                "id": r.get::<_, String>(0)?,
                                "started_at": r.get::<_, String>(1)?,
                                "target_url": r.get::<_, String>(2)?,
                                "via": r.get::<_, String>(3)?,
                                "cache_mode": r.get::<_, String>(4)?,
                                "label": r.get::<_, Option<String>>(5)?,
                            }))
                        })?
                        .collect::<rusqlite::Result<_>>()?;
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                    Ok(())
                }
                SsotCmd::Trace { id } => {
                    let mut st = store.conn().prepare(
                        "SELECT seq, method, url, status, wire_bytes, decoded_bytes,
                                content_encoding, us_ttfb, us_total, from_cache
                           FROM exchange WHERE trace_id = ?1 ORDER BY seq",
                    )?;
                    let rows: Vec<serde_json::Value> = st
                        .query_map(rusqlite::params![id], |r| {
                            Ok(serde_json::json!({
                                "seq": r.get::<_, i64>(0)?,
                                "method": r.get::<_, String>(1)?,
                                "url": r.get::<_, String>(2)?,
                                "status": r.get::<_, Option<i64>>(3)?,
                                "wire_bytes": r.get::<_, Option<i64>>(4)?,
                                "decoded_bytes": r.get::<_, Option<i64>>(5)?,
                                "content_encoding": r.get::<_, Option<String>>(6)?,
                                "us_ttfb": r.get::<_, Option<i64>>(7)?,
                                "us_total": r.get::<_, Option<i64>>(8)?,
                                "from_cache": r.get::<_, i64>(9)? != 0,
                            }))
                        })?
                        .collect::<rusqlite::Result<_>>()?;
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                    Ok(())
                }
                SsotCmd::Compression { url } => {
                    let (sql, p): (&str, Vec<String>) = match url {
                        Some(u) => (
                            "SELECT url, content_encoding, wire_bytes, decoded_bytes,
                                    compressed, saved_fraction
                               FROM v_compression WHERE url = ?1",
                            vec![u.clone()],
                        ),
                        None => (
                            "SELECT url, content_encoding, wire_bytes, decoded_bytes,
                                    compressed, saved_fraction
                               FROM v_compression LIMIT 200",
                            vec![],
                        ),
                    };
                    let mut st = store.conn().prepare(sql)?;
                    let rows: Vec<serde_json::Value> = st
                        .query_map(rusqlite::params_from_iter(p.iter()), |r| {
                            Ok(serde_json::json!({
                                "url": r.get::<_, String>(0)?,
                                "content_encoding": r.get::<_, Option<String>>(1)?,
                                "wire_bytes": r.get::<_, Option<i64>>(2)?,
                                "decoded_bytes": r.get::<_, Option<i64>>(3)?,
                                "compressed": r.get::<_, Option<i64>>(4)?.map(|v| v != 0),
                                "saved_fraction": r.get::<_, Option<f64>>(5)?,
                            }))
                        })?
                        .collect::<rusqlite::Result<_>>()?;
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                    Ok(())
                }
            }
        }
    }
}

fn parse_transport(s: &str) -> Result<Transport> {
    Transport::parse(s).ok_or_else(|| {
        anyhow!(
            "unknown transport {s:?}. Gecko speaks marionette, rdp and bidi. \
             CDP is a Blink protocol and is not present in Gecko."
        )
    })
}

fn parse_host_port(s: &str) -> Result<(String, u16)> {
    let (h, p) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("{s:?} is not host:port"))?;
    Ok((
        h.to_string(),
        p.parse().with_context(|| format!("port in {s:?}"))?,
    ))
}

fn moz_log_spec(v: &Option<String>) -> Option<String> {
    v.as_ref().map(|s| {
        if s == "default" {
            lwdbg_transports::mozlog::default_spec()
        } else {
            s.clone()
        }
    })
}

fn run_browser_measure(store: &mut Store, m: &MeasureArgs, home: &Path, json: bool) -> Result<()> {
    let transport = parse_transport(&m.via)?;
    let binary = m.browser.clone().ok_or_else(|| {
        anyhow!(
            "--via {} needs --browser <path>; `lwdbg detect` lists what is installed",
            m.via
        )
    })?;
    let name = binary
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "unknown".into());

    let o = browser::Opts {
        binary,
        browser_name: name,
        url: m.url.clone(),
        transport,
        headless: !m.headed,
        runs: m.runs,
        proxy: match &m.proxy {
            Some(p) => Some(parse_host_port(p)?),
            None => None,
        },
        moz_log: moz_log_spec(&m.moz_log),
        profile_root: profile_root(home)?,
        screenshot: m.screenshot,
        full_page: m.full_page,
        keep_profile: m.keep_profile,
        width: 1280,
        height: 800,
        label: m.label.clone(),
        timeout: std::time::Duration::from_secs(m.timeout),
    };

    let s = browser::measure(store, &o)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }

    println!(
        "{} in {} via {}{}",
        s.url,
        s.browser,
        s.via,
        if s.headless { " (headless)" } else { "" }
    );
    for r in &s.runs {
        match &r.error {
            Some(e) => println!("  run{} FAILED {e}", r.run),
            None => println!(
                "  run{} TTFB={} DCL={} load={} wire={} decoded={} resources={} log={}",
                r.run,
                fmt_us(r.us_ttfb),
                fmt_us(r.us_dcl),
                fmt_us(r.us_load),
                r.encoded_body_size
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into()),
                r.decoded_body_size
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into()),
                r.resources,
                r.log_lines,
            ),
        }
    }
    if let Some(t) = s.median_us_ttfb {
        println!("  median TTFB  {:.3}s", t / 1e6);
    }
    if let Some(t) = s.median_us_load {
        println!("  median load  {:.3}s", t / 1e6);
    }
    if !s.note.is_empty() {
        println!("  note: {}", s.note);
    }
    println!(
        "  compare against the non-browser baseline: lwdbg measure {} --via curl",
        s.url
    );
    Ok(())
}

fn fmt_us(v: Option<i64>) -> String {
    match v {
        Some(us) => format!("{:.3}s", us as f64 / 1e6),
        None => "?".into(),
    }
}

fn run_attach(a: &AttachArgs, json: bool) -> Result<()> {
    let t = parse_transport(&a.transport)?;
    let port = a.port.unwrap_or_else(|| t.default_port());
    let timeout = std::time::Duration::from_secs(a.timeout);
    let addr = format!("{}:{port}", a.host);

    if !t.attaches_to_running() {
        eprintln!(
            "note: {} cannot be enabled on a running process. This will only \
             connect if the browser was started with the flag.",
            t.as_str()
        );
    }

    let report = match t {
        Transport::Rdp => {
            let mut r = lwdbg_transports::rdp::Rdp::connect(&addr, timeout)?;
            let described = r.describe()?;
            let tabs = r.list_tabs().unwrap_or_default();
            serde_json::json!({
                "transport": "rdp",
                "address": addr,
                "attached_to_running_browser": true,
                "describe": described,
                "tabs": tabs,
            })
        }
        Transport::Marionette => {
            let mut m = lwdbg_transports::marionette::Marionette::connect(&addr, timeout)?;
            let caps = m.new_session()?.clone();
            let handles = m.window_handles().unwrap_or_default();
            let url = m.current_url().unwrap_or_default();
            m.delete_session().ok();
            serde_json::json!({
                "transport": "marionette",
                "address": addr,
                "application_type": m.application_type,
                "protocol": m.protocol,
                "capabilities": caps,
                "window_handles": handles,
                "current_url": url,
            })
        }
        Transport::Bidi => {
            let mut b = lwdbg_transports::bidi::Bidi::connect(&a.host, port, timeout)?;
            let session = b.new_session()?;
            let contexts = b.contexts().unwrap_or_default();
            let ep = b.endpoint.clone();
            b.end_session().ok();
            serde_json::json!({
                "transport": "bidi",
                "endpoint": ep,
                "session": session,
                "contexts": contexts,
            })
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
        if t == Transport::Rdp {
            let n = report["tabs"].as_array().map(|a| a.len()).unwrap_or(0);
            println!("\n{n} tab(s). This is a live session: do not navigate it away from under the user.");
        }
    }
    Ok(())
}

fn run_launch(l: &LaunchArgs, home: &Path, json: bool) -> Result<()> {
    let mut want = Vec::new();
    for t in &l.transports {
        want.push(parse_transport(t)?);
    }
    let binary = lwdbg_transports::launch::resolve_binary(&l.browser)?;
    let profile = l.profile.clone().unwrap_or_else(|| {
        home.join("profiles")
            .join(format!("launch-{}", std::process::id()))
    });

    let pick = |t: Transport| -> Result<Option<u16>> {
        if !want.contains(&t) {
            return Ok(None);
        }
        let s = std::net::TcpListener::bind("127.0.0.1:0")?;
        let p = s.local_addr()?.port();
        drop(s);
        Ok(Some(p))
    };

    let spec = lwdbg_transports::launch::LaunchSpec {
        binary: binary.clone(),
        profile_dir: profile.clone(),
        headless: !l.headed,
        marionette_port: pick(Transport::Marionette)?,
        rdp_port: pick(Transport::Rdp)?,
        bidi_port: pick(Transport::Bidi)?,
        proxy: match &l.proxy {
            Some(p) => {
                let (host, port) = parse_host_port(p)?;
                Some(lwdbg_transports::launch::ProxyPrefs {
                    host,
                    port,
                    include_localhost: true,
                })
            }
            None => None,
        },
        moz_log: moz_log_spec(&l.moz_log),
        log_file: l.moz_log.as_ref().map(|_| profile.with_extension("log")),
        extra_args: vec![],
        url: l.url.clone(),
    };

    let argv = lwdbg_transports::launch::args(&spec);
    let info = serde_json::json!({
        "binary": binary.display().to_string(),
        "profile": profile.display().to_string(),
        "marionette_port": spec.marionette_port,
        "rdp_port": spec.rdp_port,
        "bidi_port": spec.bidi_port,
        "log_file": spec.log_file.as_ref().map(|p| p.display().to_string()),
        "argv": argv,
    });

    if l.print_only {
        lwdbg_transports::launch::prepare_profile(&profile, &spec)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&info)?);
        } else {
            println!("profile prepared: {}", profile.display());
            println!("user.js written with the debug prefs. Run it yourself with:\n");
            println!("{} \\", shell_quote(&binary.display().to_string()));
            println!(
                "  {}",
                argv.iter()
                    .map(|a| shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            println!(
                "\nthen: lwdbg attach --transport rdp --port {:?}",
                spec.rdp_port
            );
        }
        return Ok(());
    }

    let running = lwdbg_transports::launch::spawn(spec)?;
    let mut ready = serde_json::Map::new();
    for (name, port) in [
        ("marionette", running.spec.marionette_port),
        ("rdp", running.spec.rdp_port),
        ("bidi", running.spec.bidi_port),
    ] {
        if let Some(p) = port {
            let r = lwdbg_transports::launch::wait_for_port(p, std::time::Duration::from_secs(60));
            ready.insert(
                name.to_string(),
                match r {
                    Ok(()) => serde_json::json!({ "port": p, "listening": true }),
                    Err(e) => {
                        serde_json::json!({ "port": p, "listening": false, "error": e.to_string() })
                    }
                },
            );
        }
    }

    let mut out = info;
    out["pid"] = serde_json::json!(running.child.id());
    out["transports"] = Value::Object(ready);
    println!("{}", serde_json::to_string_pretty(&out)?);
    println!(
        "\nThe browser is running. Attach with `lwdbg attach`, then stop it with \
         `kill {}`. The profile is scratch and can be deleted.",
        running.child.id()
    );
    // Deliberately not shut down: the point of `launch` is to leave it up.
    std::mem::forget(running.child);
    Ok(())
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || "._-/:=".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Android output is JSON regardless of `--json`: every result here is a
/// device record, a socket list or a blob reference, and there is no useful
/// prose rendering of those. The flag is accepted so the CLI is uniform.
fn run_android(store_dir: &PathBuf, a: &AndroidArgs, _json: bool) -> Result<()> {
    let serial = a.serial.as_deref();
    let out = |v: Value| -> Result<()> {
        println!("{}", serde_json::to_string_pretty(&v)?);
        Ok(())
    };

    match &a.cmd {
        AndroidCmd::Devices => {
            let d = android::devices()?;
            if d.is_empty() {
                println!(
                    "no devices. Checklist, in the order things usually go wrong:\n\
                     \n  1. Developer options enabled on the device\n\
                     \n  2. USB debugging enabled in Developer options\n\
                     \n  3. the cable in file-transfer mode, not charging-only\n\
                     \n  4. Remote debugging via USB enabled in Firefox's own settings\n\
                     \n  5. the Allow prompt accepted on the device screen"
                );
                return Ok(());
            }
            out(serde_json::to_value(&d)?)
        }

        AndroidCmd::Sockets => {
            let s = android::firefox_sockets(serial)?;
            if s.is_empty() {
                println!(
                    "no Firefox debugger socket on the device. Firefox must be \
                     running, and \"Remote debugging via USB\" must be on in its \
                     settings — that switch is separate from Android's USB debugging."
                );
                return Ok(());
            }
            out(serde_json::to_value(&s)?)
        }

        AndroidCmd::Attach { socket, port, keep } => {
            let name = match socket {
                Some(s) => s.clone(),
                None => {
                    android::firefox_sockets(serial)?
                        .into_iter()
                        .next()
                        .ok_or_else(|| {
                            anyhow!("no debugger socket found; run `lwdbg android sockets`")
                        })?
                        .name
                }
            };
            android::forward(serial, *port, &name)?;
            let result = (|| -> Result<Value> {
                let mut r = lwdbg_transports::rdp::Rdp::connect(
                    &format!("127.0.0.1:{port}"),
                    std::time::Duration::from_secs(20),
                )?;
                let described = r.describe()?;
                let tabs = r.list_tabs().unwrap_or_default();
                Ok(serde_json::json!({
                    "socket": name,
                    "local_port": port,
                    "describe": described,
                    "tabs": tabs,
                }))
            })();
            if !*keep {
                android::remove_forward(serial, *port).ok();
            }
            let v = result?;
            out(v)?;
            if *keep {
                println!(
                    "\nForward left in place on 127.0.0.1:{port}. The same RDP client \
                     the desktop path uses works against it:\n  \
                     lwdbg attach --transport rdp --port {port}\n  \
                     lwdbg eval --transport rdp --port {port} 'return document.title;'"
                );
            }
            Ok(())
        }

        AndroidCmd::Size => {
            let (w, h) = android::screen_size(serial)?;
            out(serde_json::json!({ "width": w, "height": h }))
        }

        AndroidCmd::Tap { x, y } => {
            android::tap(serial, *x, *y)?;
            out(serde_json::json!({ "tapped": [x, y] }))
        }

        AndroidCmd::Swipe { x1, y1, x2, y2, ms } => {
            android::swipe(serial, *x1, *y1, *x2, *y2, *ms)?;
            out(serde_json::json!({ "swiped": [[x1, y1], [x2, y2]], "ms": ms }))
        }

        AndroidCmd::Text { text } => {
            android::text(serial, text)?;
            out(serde_json::json!({ "typed": text }))
        }

        AndroidCmd::Key { key } => {
            android::keyevent(serial, key)?;
            out(serde_json::json!({ "key": key }))
        }

        AndroidCmd::Screenshot => {
            let png = android::screencap(serial)?;
            let (w, h) = browser::png_dimensions(&png).unwrap_or((0, 0));
            let store = Store::open(store_dir)?;
            let hash = store.put_blob(&png, Some("image/png"))?;
            let trace = store.begin_trace(&lwdbg_ssot::NewTrace {
                target_url: "android:screen".into(),
                browser: Some("android".into()),
                browser_version: None,
                engine: Some("gecko".into()),
                host: "device".into(),
                via: "adb".into(),
                headless: false,
                cache_mode: "bypass".into(),
                label: Some("adb screencap".into()),
            })?;
            let rid = store.record_render(
                &trace,
                &lwdbg_ssot::Render {
                    width: w as i64,
                    height: h as i64,
                    dpr: 1.0,
                    full_page: false,
                    blob_hash: hash.clone(),
                },
            )?;
            store.finish_trace(&trace)?;
            out(serde_json::json!({
                "render_id": rid, "blob": hash, "width": w, "height": h,
                "bytes": png.len()
            }))
        }

        AndroidCmd::Proxy {
            host_port,
            device_port,
        } => {
            let dp = device_port.unwrap_or(*host_port);
            android::reverse_proxy(serial, dp, *host_port)?;
            out(serde_json::json!({
                "device_port": dp,
                "host_port": host_port,
                "next": format!(
                    "In Firefox on the device set the HTTP and HTTPS proxy to \
                     127.0.0.1:{dp}. adb tunnels it back here. The proxy's CA must \
                     also be trusted on the device, through \
                     security.enterprise_roots.enabled plus an Android-level \
                     certificate install."
                ),
            }))
        }
    }
}

const NATIVE_HOST_NAME: &str = "net.lizicska.lwdbg";

/// Both candidate native-messaging directories.
///
/// Gecko reads a *vendor-specific* path, and the LibreWolf binary carries both
/// the "LibreWolf" and the "Mozilla" vendor string, so which one it uses
/// cannot be read off the bundle. Writing both and letting the running
/// instance choose is the only reliable answer; a tool that writes only the
/// Mozilla path fails silently here.
fn native_messaging_dirs(home: &Path) -> Vec<PathBuf> {
    let base = home.join("Library/Application Support");
    ["Mozilla", "LibreWolf", "Firefox"]
        .iter()
        .map(|v| base.join(v).join("NativeMessagingHosts"))
        .collect()
}

/// The directory that holds `skills/`, `extension/` and `ssot/`.
///
/// The same derivation serves both layouts, which is why the install layout
/// was chosen to mirror the repository:
///
///   repo:      <repo>/skills/librewolf-debugger        -> <repo>
///   installed: $PREFIX/share/lwdbg/skills/librewolf-debugger
///                                                      -> $PREFIX/share/lwdbg
///
/// `extension/` sits beside `skills/` in both, so nothing here needs to know
/// which one it is looking at.
fn data_root() -> Result<PathBuf> {
    let skill = adapters::find_skill_dir()?;
    skill
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("cannot locate the data root from {}", skill.display()))
}

fn live_socket(home_store: &Path) -> PathBuf {
    home_store.join("live.sock")
}

fn run_live(store: &mut Store, l: &LiveArgs, home: &Path, json: bool) -> Result<()> {
    let user_home = user_home()?;

    match &l.cmd {
        LiveCmd::Install { explain } => {
            let repo = data_root()?;
            let host_bin = ["target/release/lwdbg-host", "target/debug/lwdbg-host"]
                .iter()
                .map(|p| repo.join(p))
                .find(|p| p.is_file())
                .or_else(|| {
                    // The target directory may be relocated (it is on this
                    // machine, because the repo volume is too slow to build on).
                    std::env::var_os("CARGO_TARGET_DIR").and_then(|d| {
                        let d = PathBuf::from(d);
                        ["release/lwdbg-host", "debug/lwdbg-host"]
                            .iter()
                            .map(|p| d.join(p))
                            .find(|p| p.is_file())
                    })
                })
                .or_else(|| {
                    // Installed layout: both binaries land in the same
                    // bindir, so the sibling of the running executable is the
                    // most reliable answer available.
                    // Used to locate this program's own data files, not to make an
                    // authorization decision. Anyone who can replace the executable
                    // already has everything this would protect.
                    // nosemgrep: rust.lang.security.current-exe.current-exe
                    std::env::current_exe().ok().and_then(|e| {
                        let p = e.with_file_name("lwdbg-host");
                        p.is_file().then_some(p)
                    })
                })
                .or_else(|| which_in_path("lwdbg-host"))
                .ok_or_else(|| {
                    anyhow!(
                        "lwdbg-host is not built. Run `cargo build --release -p lwdbg-host` \
                         in {}, or put lwdbg-host on PATH.",
                        repo.display()
                    )
                })?;

            let manifest = serde_json::json!({
                "name": NATIVE_HOST_NAME,
                "description": "lwdbg live bridge",
                "path": host_bin.display().to_string(),
                "type": "stdio",
                "allowed_extensions": ["lwdbg-live@localhost"],
            });

            let mut written = Vec::new();
            for dir in native_messaging_dirs(&user_home) {
                // Only write where the vendor directory already exists, plus
                // Mozilla which is always a candidate. Creating a directory
                // for a browser that is not installed is litter.
                let vendor_exists = dir.parent().map(|p| p.is_dir()).unwrap_or(false);
                if !vendor_exists && !dir.to_string_lossy().contains("Mozilla") {
                    continue;
                }
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                let f = dir.join(format!("{NATIVE_HOST_NAME}.json"));
                std::fs::write(
                    &f,
                    format!("{}\n", serde_json::to_string_pretty(&manifest)?),
                )
                .with_context(|| format!("writing {}", f.display()))?;
                written.push(f.display().to_string());
            }

            let ext_dir = repo.join("extension");
            let out = serde_json::json!({
                "host_binary": host_bin.display().to_string(),
                "manifests_written": written,
                "extension_dir": ext_dir.display().to_string(),
                "socket": live_socket(&home.join("store")).display().to_string(),
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("host binary   {}", host_bin.display());
                for w in &written {
                    println!("manifest      {w}");
                }
                println!("extension     {}", ext_dir.display());
                println!(
                    "\nThe manifest is written to every candidate vendor directory on \
                     purpose: which one LibreWolf reads cannot be determined from the \
                     bundle, and the running instance decides."
                );
            }
            if *explain || !json {
                println!(
                    "\nNext, load the extension into the running browser. \
                     Unsigned is fine for this and survives until restart:\n\
                     \n  1. open about:debugging#/runtime/this-firefox\n\
                     \n  2. \"Load Temporary Add-on...\"\n\
                     \n  3. select {}/manifest.json\n\
                     \nIt is deliberately temporary. A permanent install needs \
                     xpinstall.signatures.required to be false, or an AMO unlisted \
                     signature — see references/extension.md.",
                    ext_dir.display()
                );
            }
            Ok(())
        }

        LiveCmd::Uninstall => {
            let mut removed = Vec::new();
            for dir in native_messaging_dirs(&user_home) {
                let f = dir.join(format!("{NATIVE_HOST_NAME}.json"));
                if f.exists() {
                    std::fs::remove_file(&f)?;
                    removed.push(f.display().to_string());
                }
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "removed": removed }))?
            );
            Ok(())
        }

        // Three reads that differ only in the command name. Spelling the
        // seven arguments out three times was an invitation for them to drift
        // apart; `allow_write: false` in particular must stay false here.
        LiveCmd::Commands => live_read(store, home, "commands", json),
        LiveCmd::Tabs => live_read(store, home, "tabs", json),
        LiveCmd::Windows => live_read(store, home, "windows", json),
        LiveCmd::Timing { tab } => live_print(
            store,
            home,
            "timing",
            serde_json::json!({ "tabId": tab }),
            false,
            json,
            None,
        ),
        LiveCmd::Screenshot { tab } => live_print(
            store,
            home,
            "screenshot",
            serde_json::json!({ "tabId": tab }),
            false,
            json,
            Some("png_base64"),
        ),
        LiveCmd::Call {
            command,
            params,
            allow_write,
        } => {
            let p: Value = serde_json::from_str(params)
                .with_context(|| format!("--params is not JSON: {params}"))?;
            live_print(
                store,
                home,
                command,
                p,
                *allow_write,
                json,
                Some("png_base64"),
            )
        }
    }
}

fn which_in_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// A `live` command that takes no parameters and changes nothing.
///
/// `allow_write` is hard-coded false rather than passed in: every caller of
/// this helper is a read, and a parameter would be one refactor away from
/// letting a mutating command through by accident.
fn live_read(store: &mut Store, home: &Path, command: &str, json: bool) -> Result<()> {
    live_print(
        store,
        home,
        command,
        serde_json::json!({}),
        false,
        json,
        None,
    )
}

/// Send one command over the host's unix socket and print the reply. A
/// base64 PNG in the reply is stored as a blob instead of being dumped into
/// the terminal.
fn live_print(
    store: &mut Store,
    home: &Path,
    command: &str,
    params: Value,
    allow_write: bool,
    json: bool,
    png_field: Option<&str>,
) -> Result<()> {
    let sock = live_socket(&home.join("store"));
    let mut stream = std::os::unix::net::UnixStream::connect(&sock).with_context(|| {
        format!(
            "cannot reach the live bridge at {}.\n\
             The host is started by the browser, not by this command, so this \
             means the extension is not loaded or not connected. Run \
             `lwdbg live install` and load the extension.",
            sock.display()
        )
    })?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(180)))?;

    let req = serde_json::json!({
        "command": command,
        "params": params,
        "allow_write": allow_write,
    });
    use std::io::{Read, Write};
    stream.write_all(serde_json::to_vec(&req)?.as_slice())?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    let mut reply: Value = serde_json::from_slice(&buf)
        .with_context(|| format!("reply is not JSON: {:?}", String::from_utf8_lossy(&buf)))?;

    // Keep a screenshot out of the terminal and in the store, where the
    // parity tooling can reach it.
    if let Some(field) = png_field {
        if let Some(b64) = reply
            .pointer(&format!("/result/{field}"))
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            let bytes = browser::decode_b64(&b64)?;
            let (w, h) = browser::png_dimensions(&bytes).unwrap_or((0, 0));
            let hash = store.put_blob(&bytes, Some("image/png"))?;
            let trace = store.begin_trace(&lwdbg_ssot::NewTrace {
                target_url: reply
                    .pointer("/result/url")
                    .and_then(Value::as_str)
                    .unwrap_or("live")
                    .to_string(),
                browser: Some("live".into()),
                browser_version: None,
                engine: Some("gecko".into()),
                host: "local".into(),
                via: "extension".into(),
                headless: false,
                cache_mode: "bypass".into(),
                label: Some("live screenshot".into()),
            })?;
            let rid = store.record_render(
                &trace,
                &lwdbg_ssot::Render {
                    width: w as i64,
                    height: h as i64,
                    dpr: 1.0,
                    full_page: false,
                    blob_hash: hash.clone(),
                },
            )?;
            store.finish_trace(&trace)?;
            if let Some(o) = reply.pointer_mut("/result").and_then(Value::as_object_mut) {
                o.remove(field);
                o.insert("render_id".into(), serde_json::json!(rid));
                o.insert("blob".into(), serde_json::json!(hash));
                o.insert("bytes".into(), serde_json::json!(bytes.len()));
            }
        }
    }

    println!("{}", serde_json::to_string_pretty(&reply)?);
    if !json && reply.get("ok").and_then(Value::as_bool) == Some(false) {
        std::process::exit(1);
    }
    Ok(())
}

/// The separate GPU binary, if it is installed.
///
/// Looked for beside this executable first — `make install` puts both in the
/// same bindir — then on PATH. Returns None rather than erroring: the CPU
/// path is the reference implementation, so its absence is a capability
/// question, not a failure.
fn gpu_helper() -> Option<PathBuf> {
    // Used to locate this program's own data files, not to make an
    // authorization decision. Anyone who can replace the executable
    // already has everything this would protect.
    // nosemgrep: rust.lang.security.current-exe.current-exe
    if let Ok(exe) = std::env::current_exe() {
        let p = exe.with_file_name("lwdbg-gpu");
        if p.is_file() {
            return Some(p);
        }
    }
    which_in_path("lwdbg-gpu")
}

/// Compare two PNGs, delegating to `lwdbg-gpu` when the GPU is wanted.
///
/// The split exists because linking wgpu and naga into this binary is what
/// made its release build unfinishable on an 8 GiB machine. The cost is this
/// function: two temporary files and a subprocess, which is nothing next to
/// the GPU work itself, and `lwdbg` keeps a dependency tree with zero wgpu
/// in it.
fn compare_pngs(
    a: &[u8],
    b: &[u8],
    backend: lwdbg_gpu::Backend,
    home: &Path,
) -> Result<lwdbg_gpu::Parity> {
    if backend == lwdbg_gpu::Backend::Cpu {
        return lwdbg_gpu::compare_png(a, b, backend);
    }

    let helper = match gpu_helper() {
        Some(h) => h,
        None => {
            if backend == lwdbg_gpu::Backend::Gpu {
                return Err(anyhow!(
                    "--backend gpu needs the lwdbg-gpu helper, which is not \
                     installed. Build it with `./configure --with-gpu && make`, \
                     or use --backend cpu: the CPU path is the reference \
                     implementation the shader is tested against, so nothing \
                     is lost but speed on large batches."
                ));
            }
            // Auto: the documented fallback.
            return lwdbg_gpu::compare_png(a, b, lwdbg_gpu::Backend::Cpu);
        }
    };

    let scratch = home.join("scratch");
    std::fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
    let pa = scratch.join(format!("parity-a-{}.png", std::process::id()));
    let pb = scratch.join(format!("parity-b-{}.png", std::process::id()));
    std::fs::write(&pa, a)?;
    std::fs::write(&pb, b)?;

    let out = std::process::Command::new(&helper)
        .arg("compare")
        .arg(&pa)
        .arg(&pb)
        .arg("--backend")
        .arg(if backend == lwdbg_gpu::Backend::Gpu {
            "gpu"
        } else {
            "auto"
        })
        .arg("--json")
        .output()
        .with_context(|| format!("running {}", helper.display()))?;
    std::fs::remove_file(&pa).ok();
    std::fs::remove_file(&pb).ok();

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if backend == lwdbg_gpu::Backend::Auto {
            // A helper that exists but cannot run — no adapter, a driver
            // problem — must not take the measurement with it.
            return lwdbg_gpu::compare_png(a, b, lwdbg_gpu::Backend::Cpu);
        }
        return Err(anyhow!("lwdbg-gpu failed: {err}"));
    }

    serde_json::from_slice(&out.stdout).with_context(|| {
        format!(
            "parsing lwdbg-gpu output: {:.300}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn run_parity(store: &mut Store, a: &ParityArgs, home: &Path, json: bool) -> Result<()> {
    let backend = lwdbg_gpu::Backend::parse(&a.backend)
        .ok_or_else(|| anyhow!("backend must be auto, gpu or cpu"))?;

    // Mode 1: two renders already in the store. Nothing is launched.
    if let (Some(ia), Some(ib)) = (a.render_a, a.render_b) {
        let pa = compare_stored(store, ia, ib, backend, home)?;
        store.record_parity(
            ia,
            ib,
            Some(pa.ssim),
            Some(pa.phash_hamming as i64),
            &pa.backend,
        )?;
        print_parity(
            &[(format!("render {ia}"), format!("render {ib}"), pa)],
            json,
        )?;
        return Ok(());
    }

    let url = a.url.clone().ok_or_else(|| {
        anyhow!("pass --url with two or more --browser, or --render-a/--render-b")
    })?;
    if a.browsers.len() < 2 {
        return Err(anyhow!(
            "parity needs at least two browsers; `lwdbg detect` lists what is installed. \
             Note that only Gecko browsers can be driven here — a Blink or WebKit \
             browser has no transport in this build and its render must be supplied \
             as a stored PNG."
        ));
    }
    let transport = parse_transport(&a.via)?;

    // Render each browser once, keeping the render row id.
    let mut shots: Vec<(String, i64, Vec<u8>)> = Vec::new();
    for b in &a.browsers {
        let name = b
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| "unknown".into());
        let o = browser::Opts {
            binary: b.clone(),
            browser_name: name.clone(),
            url: url.clone(),
            transport,
            headless: true,
            runs: 1,
            proxy: None,
            moz_log: None,
            profile_root: profile_root(home)?,
            screenshot: true,
            full_page: a.full_page,
            keep_profile: false,
            width: 1280,
            height: 800,
            label: Some(format!("parity {name}")),
            timeout: std::time::Duration::from_secs(a.timeout),
        };
        let s = browser::measure(store, &o)?;
        let run = s
            .runs
            .first()
            .ok_or_else(|| anyhow!("{name}: no run produced"))?;
        if let Some(e) = &run.error {
            return Err(anyhow!("{name}: {e}"));
        }
        let rid = run
            .render_id
            .ok_or_else(|| anyhow!("{name}: no screenshot captured"))?;
        let png = render_png(store, rid)?;
        shots.push((name, rid, png));
    }

    // Every pair, so a three-way comparison shows which one is the outlier.
    let mut rows = Vec::new();
    for i in 0..shots.len() {
        for j in i + 1..shots.len() {
            let p = compare_pngs(&shots[i].2, &shots[j].2, backend, home)?;
            store.record_parity(
                shots[i].1,
                shots[j].1,
                Some(p.ssim),
                Some(p.phash_hamming as i64),
                &p.backend,
            )?;
            rows.push((shots[i].0.clone(), shots[j].0.clone(), p));
        }
    }
    print_parity(&rows, json)
}

fn render_png(store: &Store, render_id: i64) -> Result<Vec<u8>> {
    let hash: String = store
        .conn()
        .query_row(
            "SELECT blob_hash FROM render WHERE id = ?1",
            rusqlite::params![render_id],
            |r| r.get(0),
        )
        .with_context(|| format!("render {render_id} is not in the store"))?;
    store
        .read_blob(&hash)?
        .ok_or_else(|| anyhow!("render {render_id} points at blob {hash}, which is gone"))
}

fn compare_stored(
    store: &Store,
    a: i64,
    b: i64,
    backend: lwdbg_gpu::Backend,
    home: &Path,
) -> Result<lwdbg_gpu::Parity> {
    let pa = render_png(store, a)?;
    let pb = render_png(store, b)?;
    compare_pngs(&pa, &pb, backend, home)
}

fn print_parity(rows: &[(String, String, lwdbg_gpu::Parity)], json: bool) -> Result<()> {
    if json {
        let out: Vec<Value> = rows
            .iter()
            .map(|(a, b, p)| serde_json::json!({ "a": a, "b": b, "parity": p }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    for (a, b, p) in rows {
        println!("{a}  vs  {b}");
        println!(
            "  SSIM {:.4}   pHash distance {}   {}x{}   backend {}{}",
            p.ssim,
            p.phash_hamming,
            p.compared_width,
            p.compared_height,
            p.backend,
            p.adapter
                .as_ref()
                .map(|x| format!(" [{x}]"))
                .unwrap_or_default()
        );
        if let Some(m) = &p.size_mismatch {
            println!("  size  {m}");
        }
        println!("  {}", p.reading);
    }
    Ok(())
}

fn run_eval(e: &EvalArgs) -> Result<()> {
    let t = parse_transport(&e.transport)?;
    let port = e.port.unwrap_or_else(|| t.default_port());
    let timeout = std::time::Duration::from_secs(e.timeout);
    let addr = format!("{}:{port}", e.host);

    let value = match t {
        Transport::Marionette => {
            let mut m = lwdbg_transports::marionette::Marionette::connect(&addr, timeout)?;
            m.new_session()?;
            let ms = timeout.as_millis() as u64;
            m.set_timeouts(ms, ms)?;
            if let Some(u) = &e.url {
                m.navigate(u)?;
            }
            let v = m.execute(&e.script)?;
            m.delete_session().ok();
            v
        }
        Transport::Bidi => {
            let mut b = lwdbg_transports::bidi::Bidi::connect(&e.host, port, timeout)?;
            b.set_timeout(timeout)?;
            b.new_session()?;
            let ctx = b
                .contexts()?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no browsing context"))?;
            if let Some(u) = &e.url {
                b.navigate(&ctx, u, "complete")?;
            }
            let v = b.evaluate(&ctx, &format!("(() => {{ {} }})()", e.script))?;
            b.end_session().ok();
            v
        }
        Transport::Rdp => {
            let mut r = lwdbg_transports::rdp::Rdp::connect(&addr, timeout)?;
            r.set_timeout(timeout)?;
            let (_tab, console) = r.active_console()?;
            if let Some(u) = &e.url {
                r.eval(&console, &format!("location.href = {u:?}"))?;
                let deadline = std::time::Instant::now() + timeout;
                while std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    if let Ok(v) = r.eval(&console, "document.readyState") {
                        let s = v
                            .get("value")
                            .and_then(Value::as_str)
                            .or_else(|| v.as_str());
                        if s == Some("complete") {
                            break;
                        }
                    }
                }
            }
            // Stringify so one JSON path covers every value shape.
            let v = r.eval(
                &console,
                &format!("JSON.stringify((() => {{ {} }})())", e.script),
            )?;
            let s = v
                .get("value")
                .and_then(Value::as_str)
                .or_else(|| v.as_str())
                .unwrap_or("null");
            serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
        }
    };

    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn run_logs(l: &LogsArgs, store_dir: &PathBuf, json: bool) -> Result<()> {
    use lwdbg_transports::mozlog;
    match &l.cmd {
        LogsCmd::Modules => {
            if json {
                let rows: Vec<Value> = mozlog::USEFUL_MODULES
                    .iter()
                    .map(|(m, why)| serde_json::json!({ "module": m, "answers": why }))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("MOZ_LOG={}\n", mozlog::default_spec());
                for (m, why) in mozlog::USEFUL_MODULES {
                    println!("  {m:<24} {why}");
                }
                println!("\nSet it on a launch: lwdbg launch --browser <path> --moz-log default");
            }
            Ok(())
        }
        LogsCmd::Ingest { file, trace, url } => {
            let text = std::fs::read_to_string(file)
                .with_context(|| format!("reading {}", file.display()))?;
            let parsed = mozlog::parse(&text);
            let kept: Vec<lwdbg_ssot::LogLine> = parsed
                .into_iter()
                .filter(|l| match url {
                    Some(u) => l
                        .url
                        .as_deref()
                        .map(|x| x.contains(u.as_str()))
                        .unwrap_or(false),
                    None => true,
                })
                .map(|l| lwdbg_ssot::LogLine {
                    source: "moz_log".into(),
                    module: l.module,
                    level: l.level,
                    us_offset: None,
                    url: l.url,
                    message: l.message,
                })
                .collect();

            let mut store = Store::open(store_dir)?;
            let trace_id = match trace {
                Some(t) => t.clone(),
                None => store.begin_trace(&lwdbg_ssot::NewTrace {
                    target_url: url.clone().unwrap_or_else(|| file.display().to_string()),
                    browser: None,
                    browser_version: None,
                    engine: Some("gecko".into()),
                    host: "local".into(),
                    via: "moz_log".into(),
                    headless: false,
                    cache_mode: "bypass".into(),
                    label: Some(format!("ingest {}", file.display())),
                })?,
            };
            let n = store.record_log_lines(&trace_id, &kept)?;
            let out = serde_json::json!({
                "trace_id": trace_id,
                "lines_stored": n,
                "file": file.display().to_string(),
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("stored {n} lines under trace {trace_id}");
                println!(
                    "join them against the network view:\n  \
                     select l.module, l.message from log_line l where l.trace_id = '{trace_id}' \
                     and l.url is not null"
                );
            }
            Ok(())
        }
    }
}

/// Where scratch browser profiles go.
///
// Test helper. The name carries the pid and the directory is
// removed first; nothing secret is written here, and a test that
// could not use a temp dir would need a fixture in the repo.
// nosemgrep: rust.lang.security.temp-dir.temp-dir
/// **Not** `std::env::temp_dir()`. A fixed name there is a predictable path in
/// a world-writable directory, and these profiles are not empty: they hold the
/// measured page's cookies and cache, and a `user.js` that disables the
/// connection prompt. A local attacker who pre-creates the directory can read
/// all of it, or plant prefs into a browser this tool is about to launch.
///
/// The store directory is already private to the user, so it is the right
/// place, and 0700 is set explicitly rather than inherited from the umask.
fn profile_root(home: &Path) -> Result<PathBuf> {
    let d = home.join("profiles");
    std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing {}", d.display()))?;
    }
    Ok(d)
}

fn user_home() -> Result<PathBuf> {
    let h = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(h))
}

fn run_adapters(a: &AdaptersArgs, json: bool) -> Result<()> {
    let skill_dir = adapters::find_skill_dir()?;
    let canon = adapters::load_canon(&skill_dir)?;
    let home = user_home()?;

    match &a.cmd {
        AdaptersCmd::List => {
            let rows: Vec<adapters::Status> = adapters::HARNESSES
                .iter()
                .map(|h| adapters::status(h, &home, &canon))
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("canonical source  {}", canon.dir.display());
                println!(
                    "binary            {}\n",
                    canon
                        .bin
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(not built)".into())
                );
                for r in &rows {
                    println!(
                        "{} {:<12} {:<28} {}",
                        if r.harness_present {
                            "present"
                        } else {
                            "absent "
                        },
                        r.id,
                        r.label,
                        r.state
                    );
                    println!("    target {}", r.target);
                    if !r.path_verified {
                        println!("    path   INFERRED — {}", r.note);
                    }
                }
            }
            Ok(())
        }

        AdaptersCmd::Emit { harness } => {
            let h = adapters::harness(harness)?;
            let files = adapters::emit(h, &canon)?;
            if files.is_empty() {
                println!(
                    "{} reads SKILL.md directories directly — nothing to generate.\n\
                     Point it at: {}",
                    h.label,
                    canon.dir.display()
                );
                return Ok(());
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&files)?);
            } else {
                for f in &files {
                    if !f.rel.is_empty() {
                        println!("===== {} =====", f.rel);
                    }
                    print!("{}", f.contents);
                    println!();
                }
            }
            Ok(())
        }

        AdaptersCmd::Install {
            harness,
            all,
            out,
            copy,
            force,
            dry_run,
        } => {
            let chosen = select(harness, *all, &home, &canon)?;
            let mut reports = Vec::new();
            let mut failures = Vec::new();
            for h in chosen {
                match adapters::install(h, &home, &canon, out.as_deref(), *copy, *force, *dry_run) {
                    Ok(r) => reports.push(r),
                    Err(e) => failures.push(format!("{}: {e}", h.id)),
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "dry_run": *dry_run,
                        "installed": reports,
                        "failed": failures,
                    }))?
                );
            } else {
                for r in &reports {
                    println!("{:<12} {:<18} {}", r.id, r.action, r.target);
                    if let Some(w) = &r.warning {
                        println!("             warning: {w}");
                    }
                }
                for f in &failures {
                    println!("FAILED       {f}");
                }
                if *dry_run {
                    println!("\n(dry run — nothing was written)");
                }
            }
            if !failures.is_empty() {
                std::process::exit(1);
            }
            Ok(())
        }

        AdaptersCmd::Uninstall {
            harness,
            all,
            out,
            dry_run,
        } => {
            let chosen = select(harness, *all, &home, &canon)?;
            let mut reports = Vec::new();
            for h in chosen {
                reports.push(adapters::uninstall(h, &home, out.as_deref(), *dry_run)?);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&reports)?);
            } else {
                for r in &reports {
                    println!("{:<12} {:<16} {}", r.id, r.action, r.target);
                }
            }
            Ok(())
        }
    }
}

/// Explicit ids win. `--all` means every harness actually present, and
/// deliberately skips `agents`, which has no fixed location.
fn select<'a>(
    ids: &[String],
    all: bool,
    home: &Path,
    canon: &adapters::Canon,
) -> Result<Vec<&'a adapters::Harness>> {
    if !ids.is_empty() {
        return ids.iter().map(|i| adapters::harness(i)).collect();
    }
    if !all {
        return Err(anyhow!(
            "pass --harness <id> (repeatable) or --all; `lwdbg adapters list` shows the ids"
        ));
    }
    let chosen: Vec<&adapters::Harness> = adapters::HARNESSES
        .iter()
        .filter(|h| !h.root.is_empty() && adapters::status(h, home, canon).harness_present)
        .collect();
    if chosen.is_empty() {
        return Err(anyhow!("no known harness found on this machine"));
    }
    Ok(chosen)
}

fn ca_install(ca: &lwdbg_proxy::ca::Ca, profile: &Path, ca_dir: &Path) -> Result<()> {
    if !profile.join("cert9.db").exists() {
        return Err(anyhow!(
            "{} has no cert9.db — is this a Gecko profile directory? (Gecko keeps a \
             per-profile trust store; the system keychain is not consulted)",
            profile.display()
        ));
    }
    // Not the shared temp dir: a predictable name there lets a local attacker
    // pre-create a symlink, and this write would then clobber whatever it
    // points at with the user's privileges. The CA directory is already the
    // tool's own and already holds the key.
    let tmp = ca_dir.join("ca-for-certutil.pem");
    std::fs::write(&tmp, ca.ca_cert_pem()).with_context(|| format!("writing {}", tmp.display()))?;

    let out = std::process::Command::new("certutil")
        .args(["-A", "-n", CA_NICKNAME, "-t", "CT,c,c", "-d"])
        .arg(format!("sql:{}", profile.display()))
        .arg("-i")
        .arg(&tmp)
        .output()
        .context("running certutil (brew install nss)")?;
    std::fs::remove_file(&tmp).ok();

    if !out.status.success() {
        return Err(anyhow!(
            "certutil failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn run_proxy(cfg: Config, json: bool) -> Result<()> {
    let scope = if cfg.only_hosts.is_empty() {
        "ALL HOSTS".to_string()
    } else {
        cfg.only_hosts.join(", ")
    };
    let handle = server::serve(cfg).await?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "listen": handle.local_addr.to_string(),
                "trace_id": handle.trace_id,
                "intercepting": scope,
            }))?
        );
    } else {
        println!("lwdbg proxy on {}", handle.local_addr);
        println!("  trace        {}", handle.trace_id);
        println!("  intercepting {scope}");
        if scope == "ALL HOSTS" {
            println!(
                "  warning: intercepting every host. A trusted-CA proxy reads all TLS \
                 traffic, credentials included. Prefer --only-host."
            );
        }
        println!("  stop with Ctrl-C");
    }

    tokio::signal::ctrl_c().await?;
    let n = handle.exchange_count().unwrap_or(0);
    println!("\nrecorded {n} exchanges under trace {}", handle.trace_id);
    Ok(())
}
