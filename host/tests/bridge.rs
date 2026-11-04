//! End-to-end test of the bridge with a fake extension.
//!
//! The browser half cannot be automated — loading a temporary add-on is a
//! click in `about:debugging`. Everything *below* that is testable, and this
//! stands in for the extension: it speaks native messaging framing on the
//! host's stdin/stdout and answers requests.
//!
//! What this pins down:
//!  * the framing survives a real process boundary, not just a Cursor;
//!  * the socket exists, is 0600, and routes a reply back to the right client;
//!  * the policy gate refuses a mutating command **without forwarding it**,
//!    which is the part that matters: a refusal that still reached the
//!    browser would be no protection at all.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const HOST: &str = env!("CARGO_BIN_EXE_lwdbg-host");

struct Harness {
    child: Child,
    home: std::path::PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        std::fs::remove_dir_all(&self.home).ok();
    }
}

fn write_frame(w: &mut impl Write, v: &Value) {
    let body = serde_json::to_vec(v).unwrap();
    w.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
    w.write_all(&body).unwrap();
    w.flush().unwrap();
}

fn read_frame(r: &mut impl Read) -> Value {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).expect("length prefix");
    let n = u32::from_le_bytes(len) as usize;
    let mut body = vec![0u8; n];
    r.read_exact(&mut body).expect("frame body");
    serde_json::from_slice(&body).expect("frame is JSON")
}

fn catalogue() -> Value {
    json!({
        "id": 0,
        "ok": true,
        "result": {
            "hello": "lwdbg-live",
            "commands": {
                "ping": { "mutates": false },
                "tabs": { "mutates": false },
                "navigate": { "mutates": true },
            }
        }
    })
}

fn start(tag: &str) -> (Harness, std::process::ChildStdin, std::process::ChildStdout) {
    let home = std::env::temp_dir().join(format!("lwdbg-bridge-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).unwrap();

    let mut child = Command::new(HOST)
        .env("LWDBG_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the host");

    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let h = Harness { child, home };
    (h, stdin, stdout)
}

fn wait_for_socket(home: &std::path::Path) -> std::path::PathBuf {
    let sock = home.join("live.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if sock.exists() {
            return sock;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the host never created {}", sock.display());
}

fn ask(sock: &std::path::Path, req: &Value) -> Value {
    let mut s = std::os::unix::net::UnixStream::connect(sock).expect("connecting to the socket");
    s.write_all(&serde_json::to_vec(req).unwrap()).unwrap();
    s.shutdown(std::net::Shutdown::Write).unwrap();
    let mut buf = Vec::new();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.read_to_end(&mut buf).unwrap();
    serde_json::from_slice(&buf).expect("reply is JSON")
}

#[test]
fn a_read_command_is_forwarded_and_the_reply_comes_back() {
    let (h, mut hin, mut hout) = start("read");
    write_frame(&mut hin, &catalogue());
    let sock = wait_for_socket(&h.home);

    // Issue the request from another thread; the fake extension answers here.
    let sock2 = sock.clone();
    let caller = std::thread::spawn(move || ask(&sock2, &json!({ "command": "tabs" })));

    let fwd = read_frame(&mut hout);
    assert_eq!(fwd["command"], "tabs");
    let id = fwd["id"].as_i64().expect("the host assigns an id");
    assert!(id > 0, "id 0 is reserved for the extension's hello");

    write_frame(
        &mut hin,
        &json!({ "id": id, "ok": true, "result": { "tabs": [ { "id": 3, "url": "https://example.com/" } ] } }),
    );

    let reply = caller.join().unwrap();
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["result"]["tabs"][0]["url"], "https://example.com/");
}

#[test]
fn a_mutating_command_is_refused_without_reaching_the_browser() {
    let (h, mut hin, mut hout) = start("gate");
    write_frame(&mut hin, &catalogue());
    let sock = wait_for_socket(&h.home);

    let reply = ask(
        &sock,
        &json!({ "command": "navigate", "params": { "url": "https://example.com/" } }),
    );
    assert_eq!(reply["ok"], false);
    let err = reply["error"].as_str().unwrap();
    assert!(err.contains("changes the state"), "got: {err}");
    assert!(err.contains("real session"), "got: {err}");

    // Nothing may have been forwarded. Prove it by issuing a read next and
    // checking that *it* is the first frame the browser sees.
    let sock2 = sock.clone();
    let caller = std::thread::spawn(move || ask(&sock2, &json!({ "command": "ping" })));
    let fwd = read_frame(&mut hout);
    assert_eq!(
        fwd["command"], "ping",
        "a refused command still reached the browser, so the gate is decoration"
    );
    let id = fwd["id"].as_i64().unwrap();
    write_frame(
        &mut hin,
        &json!({ "id": id, "ok": true, "result": { "pong": true } }),
    );
    assert_eq!(caller.join().unwrap()["ok"], true);
}

#[test]
fn allow_write_lets_a_mutating_command_through() {
    let (h, mut hin, mut hout) = start("allow");
    write_frame(&mut hin, &catalogue());
    let sock = wait_for_socket(&h.home);

    let sock2 = sock.clone();
    let caller = std::thread::spawn(move || {
        ask(
            &sock2,
            &json!({
                "command": "navigate",
                "params": { "url": "https://example.com/" },
                "allow_write": true
            }),
        )
    });
    let fwd = read_frame(&mut hout);
    assert_eq!(fwd["command"], "navigate");
    assert_eq!(fwd["params"]["url"], "https://example.com/");
    let id = fwd["id"].as_i64().unwrap();
    write_frame(
        &mut hin,
        &json!({ "id": id, "ok": true, "result": { "tabId": 3 } }),
    );
    assert_eq!(caller.join().unwrap()["result"]["tabId"], 3);
}

#[test]
fn an_unknown_command_is_treated_as_mutating_and_refused() {
    // A command the catalogue does not mention could do anything. Defaulting
    // to "allowed" would mean a newer extension could bypass the gate.
    let (h, mut hin, _hout) = start("unknown");
    write_frame(&mut hin, &catalogue());
    let sock = wait_for_socket(&h.home);
    let reply = ask(&sock, &json!({ "command": "somethingNew" }));
    assert_eq!(reply["ok"], false);
    assert!(reply["error"].as_str().unwrap().contains("refused"));
}

#[test]
fn the_command_catalogue_is_answered_without_touching_the_browser() {
    let (h, mut hin, _hout) = start("cat");
    write_frame(&mut hin, &catalogue());
    let sock = wait_for_socket(&h.home);
    let reply = ask(&sock, &json!({ "command": "commands" }));
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["result"]["hello"], "lwdbg-live");
    assert_eq!(reply["result"]["commands"]["navigate"]["mutates"], true);
}

#[test]
fn the_socket_is_not_readable_by_other_users() {
    use std::os::unix::fs::PermissionsExt;
    let (h, mut hin, _hout) = start("perm");
    write_frame(&mut hin, &catalogue());
    let sock = wait_for_socket(&h.home);
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "this socket is a channel into the user's browser session"
    );
}

#[test]
fn two_clients_in_flight_get_their_own_replies() {
    let (h, mut hin, mut hout) = start("concurrent");
    write_frame(&mut hin, &catalogue());
    let sock = wait_for_socket(&h.home);

    let s1 = sock.clone();
    let s2 = sock.clone();
    let a = std::thread::spawn(move || ask(&s1, &json!({ "command": "tabs" })));
    let f1 = read_frame(&mut hout);
    let b = std::thread::spawn(move || ask(&s2, &json!({ "command": "ping" })));
    let f2 = read_frame(&mut hout);

    let (id1, id2) = (f1["id"].as_i64().unwrap(), f2["id"].as_i64().unwrap());
    assert_ne!(id1, id2);

    // Answer out of order on purpose: correlation must be by id, not arrival.
    write_frame(
        &mut hin,
        &json!({ "id": id2, "ok": true, "result": { "mark": "second" } }),
    );
    write_frame(
        &mut hin,
        &json!({ "id": id1, "ok": true, "result": { "mark": "first" } }),
    );

    let ra = a.join().unwrap();
    let rb = b.join().unwrap();
    let want_a = if f1["command"] == "tabs" {
        "first"
    } else {
        "second"
    };
    assert_eq!(ra["result"]["mark"], want_a);
    assert_ne!(ra["result"]["mark"], rb["result"]["mark"]);
}
