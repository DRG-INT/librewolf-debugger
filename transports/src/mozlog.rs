//! `MOZ_LOG` parser.
//!
//! No upstream browser-agent tool folds these lines into its measurements, and
//! they are the only place some answers live: `nsHttp:5` shows the request as
//! Necko actually issued it, including whether a connection was reused, which
//! `Accept-Encoding` went out, and where a stall sat.
//!
//! Format, with `timestamp` in the spec:
//!
//! ```text
//! 2026-11-03 09:15:04.123456 UTC - [Parent 4242: Socket Thread]: V/nsHttp nsHttpChannel::Init
//! ```
//!
//! Without it the leading timestamp is simply absent. Both are handled, and a
//! line matching neither is kept verbatim at level `None` rather than dropped:
//! an unparsed log line is still evidence.

use serde::Serialize;

/// Gecko's level letters. The numbers are the `MOZ_LOG` levels, so
/// `nsHttp:5` means "up to and including Verbose".
pub fn level_of(letter: char) -> Option<i64> {
    Some(match letter {
        'E' => 1,
        'W' => 2,
        'I' => 3,
        'D' => 4,
        'V' => 5,
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogLine {
    /// Wall clock as the log printed it, when present.
    pub timestamp: Option<String>,
    pub process: Option<String>,
    pub pid: Option<i64>,
    pub thread: Option<String>,
    pub level: Option<i64>,
    pub module: Option<String>,
    pub url: Option<String>,
    pub message: String,
}

pub fn parse_line(raw: &str) -> Option<LogLine> {
    let line = raw.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() {
        return None;
    }

    let mut rest = line;
    let mut timestamp = None;

    // "<ts> UTC - " prefix, present only with the `timestamp` option.
    if let Some(i) = rest.find(" UTC - ") {
        timestamp = Some(rest[..i].trim().to_string());
        rest = &rest[i + " UTC - ".len()..];
    }

    let mut process = None;
    let mut pid = None;
    let mut thread = None;

    // "[Parent 4242: Socket Thread]: " — the thread name may contain spaces
    // and the process label may be "Parent", "Child", "GPU", ...
    if let Some(stripped) = rest.strip_prefix('[') {
        if let Some(close) = stripped.find("]: ") {
            let inside = &stripped[..close];
            let (head, tname) = match inside.split_once(": ") {
                Some((h, t)) => (h, Some(t.to_string())),
                None => (inside, None),
            };
            thread = tname;
            let mut it = head.split_whitespace();
            process = it.next().map(str::to_string);
            pid = it.next().and_then(|p| p.parse().ok());
            rest = &stripped[close + 3..];
        }
    }

    // "V/nsHttp message..."
    let mut level = None;
    let mut module = None;
    if let Some((head, tail)) = rest.split_once(' ') {
        if let Some((lvl, m)) = head.split_once('/') {
            let mut chars = lvl.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                if let Some(n) = level_of(c) {
                    level = Some(n);
                    module = Some(m.to_string());
                    rest = tail;
                }
            }
        }
    }

    Some(LogLine {
        timestamp,
        process,
        pid,
        thread,
        level,
        module,
        url: extract_url(rest),
        message: rest.to_string(),
    })
}

/// Pull the first absolute URL out of a line. This is the join key against
/// `exchange.url` and `resource_timing.url`, which is the only reason the
/// `log_line` table exists.
pub fn extract_url(s: &str) -> Option<String> {
    let start = s.find("https://").or_else(|| s.find("http://"))?;
    let tail = &s[start..];
    let end = tail
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '>' | ']' | ')' | '\\'))
        .unwrap_or(tail.len());
    let url = tail[..end].trim_end_matches([',', ';', '.']);
    (!url.is_empty()).then(|| url.to_string())
}

/// Parse a whole log file. Continuation lines — Necko prints request headers
/// one per line after the channel line — are attached to the entry above so
/// the header block stays readable.
pub fn parse(text: &str) -> Vec<LogLine> {
    let mut out: Vec<LogLine> = Vec::new();
    for raw in text.lines() {
        match parse_line(raw) {
            Some(l) => {
                let is_continuation = l.level.is_none()
                    && l.timestamp.is_none()
                    && l.pid.is_none()
                    && (raw.starts_with("  ") || raw.starts_with('\t'));
                if is_continuation {
                    if let Some(prev) = out.last_mut() {
                        prev.message.push('\n');
                        prev.message.push_str(raw.trim_end());
                        if prev.url.is_none() {
                            prev.url = extract_url(raw);
                        }
                        continue;
                    }
                }
                out.push(l);
            }
            None => continue,
        }
    }
    out
}

/// The log modules worth asking for, and why. Passed to `MOZ_LOG`.
pub const USEFUL_MODULES: &[(&str, &str)] = &[
    (
        "nsHttp:5",
        "requests as Necko issued them: headers, connection reuse, encoding",
    ),
    ("nsSocketTransport:5", "connect and DNS timing below HTTP"),
    ("nsHostResolver:4", "DNS resolution, including cache hits"),
    ("cache2:5", "the browser's own HTTP cache decisions"),
    ("CookieService:4", "cookies sent and refused"),
    (
        "nsChannelClassifier:4",
        "tracking protection blocking a request",
    ),
    (
        "TLS:4",
        "handshake failures, including a rejected proxy certificate",
    ),
    (
        "timestamp",
        "prefix every line with wall clock — needed for correlation",
    ),
    (
        "sync",
        "unbuffered writes, so a crash does not lose the tail",
    ),
];

/// The default spec: enough to explain a slow page load, not so much that the
/// logging itself becomes the bottleneck.
pub fn default_spec() -> String {
    "timestamp,sync,nsHttp:5,nsSocketTransport:4,nsHostResolver:4,cache2:4".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timestamped_necko_line_parses_fully() {
        let l = parse_line(
            "2026-11-03 09:15:04.123456 UTC - [Parent 4242: Socket Thread]: \
             V/nsHttp nsHttpChannel::Init [this=0x12ab] uri=https://neptunium.gd/index.html",
        )
        .unwrap();
        assert_eq!(l.timestamp.as_deref(), Some("2026-11-03 09:15:04.123456"));
        assert_eq!(l.process.as_deref(), Some("Parent"));
        assert_eq!(l.pid, Some(4242));
        assert_eq!(l.thread.as_deref(), Some("Socket Thread"));
        assert_eq!(l.level, Some(5));
        assert_eq!(l.module.as_deref(), Some("nsHttp"));
        assert_eq!(l.url.as_deref(), Some("https://neptunium.gd/index.html"));
    }

    #[test]
    fn an_untimestamped_line_still_parses() {
        let l = parse_line("[Child 999: Main Thread]: D/cache2 entry opened").unwrap();
        assert!(l.timestamp.is_none());
        assert_eq!(l.level, Some(4));
        assert_eq!(l.module.as_deref(), Some("cache2"));
        assert_eq!(l.message, "entry opened");
    }

    #[test]
    fn an_unrecognised_line_is_kept_verbatim_not_dropped() {
        let l = parse_line("###!!! ASSERTION: something went sideways").unwrap();
        assert!(l.level.is_none());
        assert!(l.module.is_none());
        assert_eq!(l.message, "###!!! ASSERTION: something went sideways");
    }

    #[test]
    fn a_thread_name_without_a_colon_does_not_eat_the_module() {
        let l = parse_line("[Parent 1: Main]: I/nsHttp hi").unwrap();
        assert_eq!(l.thread.as_deref(), Some("Main"));
        assert_eq!(l.module.as_deref(), Some("nsHttp"));
    }

    #[test]
    fn a_lowercase_letter_is_not_a_level() {
        // "a/b something" must not be read as level a, module b.
        let l = parse_line("[Parent 1: Main]: a/b something").unwrap();
        assert!(l.level.is_none());
        assert_eq!(l.message, "a/b something");
    }

    #[test]
    fn urls_stop_at_quotes_and_trailing_punctuation() {
        assert_eq!(
            extract_url("GET \"https://x.test/a?b=1\" done").as_deref(),
            Some("https://x.test/a?b=1")
        );
        assert_eq!(
            extract_url("redirect to https://x.test/a.").as_deref(),
            Some("https://x.test/a")
        );
        assert_eq!(extract_url("no url here"), None);
    }

    #[test]
    fn header_continuation_lines_attach_to_the_request_above() {
        let text = "\
2026-11-03 09:15:04.000000 UTC - [Parent 1: Socket Thread]: I/nsHttp nsHttpChannel::Init uri=https://neptunium.gd/
  Host: neptunium.gd
  Accept-Encoding: gzip, deflate, br
2026-11-03 09:15:05.000000 UTC - [Parent 1: Socket Thread]: I/nsHttp done
";
        let v = parse(text);
        assert_eq!(
            v.len(),
            2,
            "continuations must not become their own entries"
        );
        assert!(v[0].message.contains("Accept-Encoding: gzip, deflate, br"));
        assert_eq!(v[0].url.as_deref(), Some("https://neptunium.gd/"));
        assert_eq!(v[1].message, "done");
    }

    #[test]
    fn the_default_spec_includes_timestamps_because_correlation_needs_them() {
        let s = default_spec();
        assert!(
            s.contains("timestamp"),
            "without it nothing can be joined on time"
        );
        assert!(s.contains("nsHttp:5"));
        assert!(
            s.contains("sync"),
            "a crash must not lose the tail of the log"
        );
    }

    #[test]
    fn every_documented_module_is_explained() {
        for (m, why) in USEFUL_MODULES {
            assert!(!m.is_empty());
            assert!(why.len() > 15, "{m} has no real explanation");
        }
    }
}
