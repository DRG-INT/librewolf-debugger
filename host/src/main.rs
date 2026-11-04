//! Native messaging host: the bridge between `lwdbg` and a running browser.
//!
//! The browser starts this process; `lwdbg` cannot. So the flow is inverted
//! from what a CLI would normally do:
//!
//! ```text
//!   browser --spawns--> host  <--unix socket--  lwdbg live ...
//!              stdio             $LWDBG_HOME/live.sock
//! ```
//!
//! The host owns three things:
//!
//!  * the **framing**: native messaging is a 4-byte little-endian length
//!    prefix followed by UTF-8 JSON, on stdin and stdout. Anything written to
//!    stdout that is not a frame corrupts the channel, so every diagnostic in
//!    this binary goes to stderr. That is not a style preference: a stray
//!    `println!` here silently breaks the extension.
//!  * the **correlation**: several `lwdbg` invocations may be in flight, so
//!    each request gets an id and replies are routed back by it.
//!  * the **policy**: a command the extension marks as mutating is refused
//!    unless the caller asked for it. This browser is the user's real
//!    session, with their logins in it.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Chrome's limit is 1 MB inbound; Firefox's is larger, but a reply carrying a
/// full-page PNG as base64 is the biggest legitimate message and fits well
/// inside this.
const MAX_FRAME: usize = 192 * 1024 * 1024;

type Pending = Arc<(Mutex<HashMap<i64, Option<Value>>>, Condvar)>;

fn main() {
    if let Err(e) = run() {
        eprintln!("lwdbg-host: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let home = home_dir()?;
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    let sock_path = home.join("live.sock");

    // A socket file left behind by a previous run would make bind fail. It is
    // safe to remove: if a live host still held it, the browser would not
    // have started this one.
    std::fs::remove_file(&sock_path).ok();
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The socket is a channel into the user's browser session. Nobody
        // else on the machine gets to speak through it.
        std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("lwdbg-host: listening on {}", sock_path.display());

    let pending: Pending = Arc::new((Mutex::new(HashMap::new()), Condvar::new()));
    let next_id = Arc::new(AtomicI64::new(1));
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let catalogue: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));

    // Reader: browser -> host. Routes each reply to whoever is waiting.
    {
        let pending = Arc::clone(&pending);
        let catalogue = Arc::clone(&catalogue);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            loop {
                match read_frame(&mut stdin) {
                    Ok(Some(v)) => {
                        // id 0 is the extension's unsolicited hello.
                        if v.get("id").and_then(Value::as_i64) == Some(0) {
                            *catalogue.lock().unwrap() = v.get("result").cloned();
                            continue;
                        }
                        if let Some(id) = v.get("id").and_then(Value::as_i64) {
                            let (lock, cv) = &*pending;
                            let mut map = lock.lock().unwrap();
                            if let Some(slot) = map.get_mut(&id) {
                                *slot = Some(v);
                                cv.notify_all();
                            }
                        }
                    }
                    // Clean EOF: the browser closed the port, which is how
                    // this process is meant to end.
                    Ok(None) => {
                        eprintln!("lwdbg-host: browser closed the port");
                        std::process::exit(0);
                    }
                    Err(e) => {
                        eprintln!("lwdbg-host: reading from the browser: {e:#}");
                        std::process::exit(1);
                    }
                }
            }
        });
    }

    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("lwdbg-host: accept: {e}");
                continue;
            }
        };
        let pending = Arc::clone(&pending);
        let next_id = Arc::clone(&next_id);
        let stdout = Arc::clone(&stdout);
        let catalogue = Arc::clone(&catalogue);
        std::thread::spawn(move || {
            if let Err(e) = serve_client(conn, pending, next_id, stdout, catalogue) {
                eprintln!("lwdbg-host: client: {e:#}");
            }
        });
    }
    Ok(())
}

fn serve_client(
    mut conn: UnixStream,
    pending: Pending,
    next_id: Arc<AtomicI64>,
    stdout: Arc<Mutex<std::io::Stdout>>,
    catalogue: Arc<Mutex<Option<Value>>>,
) -> Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(300)))?;
    let mut buf = Vec::new();
    conn.read_to_end(&mut buf).context("reading the request")?;
    let req: Value = serde_json::from_slice(&buf).context("request is not JSON")?;

    let command = req
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("request has no `command`"))?
        .to_string();

    // `commands` is answered locally from what the extension announced, so
    // asking what is available never touches the browser.
    if command == "commands" {
        let c = catalogue.lock().unwrap().clone();
        let out = json!({ "ok": true, "result": c.unwrap_or(Value::Null) });
        conn.write_all(serde_json::to_string(&out)?.as_bytes())?;
        return Ok(());
    }

    // The policy gate. The extension declares which commands change state;
    // the host decides, once, whether the caller may use one.
    let allow_write = req
        .get("allow_write")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !allow_write {
        let mutating = catalogue
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|c| c.get("commands").cloned())
            .and_then(|m| m.get(&command).cloned())
            .and_then(|e| e.get("mutates").and_then(Value::as_bool));
        // Unknown is treated as mutating. If the catalogue has not arrived
        // yet, refusing is the safe answer; the alternative is acting on the
        // user's live session on the strength of a missing field.
        if mutating.unwrap_or(true) {
            let out = json!({
                "ok": false,
                "error": format!(
                    "{command:?} changes the state of a live browser session and was \
                     refused. This is the user's real session, with their logins in it. \
                     Re-send with allow_write once the action has been approved."
                ),
            });
            conn.write_all(serde_json::to_string(&out)?.as_bytes())?;
            return Ok(());
        }
    }

    let id = next_id.fetch_add(1, Ordering::SeqCst);
    {
        let (lock, _) = &*pending;
        lock.lock().unwrap().insert(id, None);
    }

    let frame = json!({
        "id": id,
        "command": command,
        "params": req.get("params").cloned().unwrap_or(json!({})),
    });
    {
        let mut out = stdout.lock().unwrap();
        write_frame(&mut *out, &frame)?;
    }

    // Wait for the reply, bounded. A browser blocked on a modal dialog would
    // otherwise hang this thread forever.
    let reply = {
        let (lock, cv) = &*pending;
        let mut map = lock.lock().unwrap();
        let deadline = Duration::from_secs(120);
        loop {
            if let Some(Some(v)) = map.get(&id).cloned() {
                map.remove(&id);
                break Some(v);
            }
            let (m, t) = cv.wait_timeout(map, deadline).unwrap();
            map = m;
            if t.timed_out() {
                map.remove(&id);
                break None;
            }
        }
    };

    let out = match reply {
        Some(v) => v,
        None => json!({
            "ok": false,
            "error": "the browser did not answer within 120s. A modal dialog \
                      blocks all extension work until it is dismissed."
        }),
    };
    conn.write_all(serde_json::to_string(&out)?.as_bytes())?;
    Ok(())
}

fn home_dir() -> Result<std::path::PathBuf> {
    if let Some(h) = std::env::var_os("LWDBG_HOME") {
        return Ok(std::path::PathBuf::from(h));
    }
    let h = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(std::path::PathBuf::from(h).join(".local/share/lwdbg"))
}

/// Native messaging framing: u32 little-endian length, then UTF-8 JSON.
pub fn read_frame(r: &mut impl Read) -> Result<Option<Value>> {
    let mut len = [0u8; 4];
    if !read_exact_or_eof(r, &mut len)? {
        return Ok(None);
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        bail!("frame claims {n} bytes, over the {MAX_FRAME} limit");
    }
    let mut body = vec![0u8; n];
    if !read_exact_or_eof(r, &mut body)? {
        bail!("truncated frame: wanted {n} bytes");
    }
    Ok(Some(serde_json::from_slice(&body).with_context(|| {
        format!(
            "frame body is not JSON: {:?}",
            String::from_utf8_lossy(&body[..body.len().min(200)])
        )
    })?))
}

pub fn write_frame(w: &mut impl Write, v: &Value) -> Result<()> {
    let body = serde_json::to_vec(v)?;
    if body.len() > MAX_FRAME {
        bail!("refusing to write a {} byte frame", body.len());
    }
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Fill `buf` completely.
///
/// Returns `false` for a clean EOF *before any byte was read*, which is how
/// the browser closing the port looks and is not an error. An EOF partway
/// through a frame is an error, because the other half of that frame is gone.
fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            if filled == 0 {
                return Ok(false);
            }
            bail!("EOF after {filled} of {} bytes", buf.len());
        }
        filled += n;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_round_trips() {
        let v = json!({"id": 7, "command": "tabs", "params": {}});
        let mut buf = Vec::new();
        write_frame(&mut buf, &v).unwrap();
        assert_eq!(
            &buf[..4],
            &(serde_json::to_vec(&v).unwrap().len() as u32).to_le_bytes()
        );
        let mut c = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut c).unwrap(), Some(v));
        assert_eq!(
            read_frame(&mut c).unwrap(),
            None,
            "clean EOF is not an error"
        );
    }

    #[test]
    fn length_is_little_endian_not_big() {
        // Getting this backwards is the classic native messaging bug: the
        // browser reads a 16-million-byte length and waits forever.
        let mut buf = Vec::new();
        write_frame(&mut buf, &json!({})).unwrap();
        assert_eq!(&buf[..4], &[2, 0, 0, 0], "{{}} is two bytes");
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_silent_empty_value() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &json!({"a": 1})).unwrap();
        buf.pop();
        let mut c = std::io::Cursor::new(buf);
        assert!(read_frame(&mut c).is_err());
    }

    #[test]
    fn an_absurd_length_is_refused_before_allocating() {
        let mut buf = vec![255u8, 255, 255, 255];
        buf.extend_from_slice(b"{}");
        let mut c = std::io::Cursor::new(buf);
        assert!(read_frame(&mut c)
            .unwrap_err()
            .to_string()
            .contains("over the"));
    }

    #[test]
    fn a_non_json_body_names_what_it_saw() {
        let body = b"not json at all";
        let mut buf = (body.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(body);
        let mut c = std::io::Cursor::new(buf);
        let e = read_frame(&mut c).unwrap_err().to_string();
        assert!(e.contains("not JSON"), "got: {e}");
    }

    #[test]
    fn an_empty_stream_is_a_clean_close() {
        let mut c = std::io::Cursor::new(Vec::new());
        assert_eq!(read_frame(&mut c).unwrap(), None);
    }
}
