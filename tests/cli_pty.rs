#![cfg(unix)]

mod harness;

use harness::tendr;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tendr::attach_proto::{
    MSG_ACCEPTED, MSG_DATA, MSG_DETACH, MSG_HELLO, MSG_INPUT_DONE, MSG_REJECTED, MSG_RESIZE,
    MSG_RETIRED, Mode, PROTOCOL_VERSION, read_msg, resize_payload,
};
use tendr::recording::Geometry;

static SERIAL: Mutex<()> = Mutex::new(());

/// Frame and send one attach message (`[type][u32 len][payload]`).
fn write_msg(stream: &mut UnixStream, msg_type: u8, payload: &[u8]) {
    stream.write_all(&[msg_type]).unwrap();
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(payload).unwrap();
    stream.flush().unwrap();
}

#[test]
fn start_pty_flag_sets_io_mode() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let output = tendr(&root)
        .args(["start", "pty-test", "--pty", "--", "echo", "hello"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let meta: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(meta["launch_spec"]["io_mode"], "Pty");
}

#[test]
fn start_pty_session_captures_output() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-echo", "--pty", "--", "echo", "pty-hello"])
        .output()
        .unwrap();

    harness::wait_terminal(&root, "pty-echo");

    let output = tendr(&root)
        .args(["log", "pty-echo", "--raw"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("pty-hello"),
        "PTY output should be captured in log: {stdout}"
    );
}

#[test]
fn start_pty_session_shows_pty_metadata() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-meta", "--pty", "--", "echo", "hi"])
        .output()
        .unwrap();

    harness::wait_terminal(&root, "pty-meta");

    let output = tendr(&root).args(["status", "pty-meta"]).output().unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let meta: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(meta["pty"]["enabled"], true);
    assert_eq!(meta["pty"]["control"], "AgentControl");
    assert_eq!(meta["launch_spec"]["io_mode"], "Pty");
}

#[test]
fn exec_rejected_on_pty_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args([
            "start",
            "pty-shell",
            "--pty",
            "--stdin",
            "--",
            "sleep",
            "60",
        ])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-shell");
    harness::wait_running(&root, "pty-shell");

    let output = tendr(&root)
        .args(["exec", "pty-shell", "--", "echo", "test"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not supported") || stderr.contains("PTY"),
        "should reject exec on PTY: {stderr}"
    );
}

#[test]
fn attach_to_non_pty_session_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pipe-session", "--", "sleep", "60"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pipe-session");
    harness::wait_running(&root, "pipe-session");

    let output = tendr(&root)
        .args(["attach", "pipe-session"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("PTY") || stderr.contains("not PTY"),
        "should reject attach on non-PTY: {stderr}"
    );
}

#[test]
fn attach_socket_exists_for_pty_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-attach", "--pty", "--", "sleep", "60"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-attach");
    harness::wait_running(&root, "pty-attach");

    let breadcrumb = root
        .path()
        .join(".tendr/sessions/default/pty-attach/a.sock.path");

    // The attach listener thread may not have written the breadcrumb yet.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !breadcrumb.exists() {
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for a.sock.path breadcrumb to appear");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        breadcrumb.exists(),
        "a.sock.path breadcrumb should exist for PTY session"
    );

    // The breadcrumb should point to an actual socket file
    let sock_path = std::fs::read_to_string(&breadcrumb).unwrap();
    let sock_path = sock_path.trim();
    assert!(
        std::path::Path::new(sock_path).exists(),
        "socket file should exist at {sock_path}"
    );
}

#[test]
fn push_to_pty_session_delivers_input() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start a PTY cat session with stdin
    tendr(&root)
        .args(["start", "pty-push", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-push");
    harness::wait_running(&root, "pty-push");

    // Push some input
    tendr(&root)
        .args(["push", "pty-push"])
        .write_stdin(b"hello-from-push\n")
        .output()
        .unwrap();

    // Poll the log until the pushed input echoes through the PTY (no fixed sleep).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let output = tendr(&root)
            .args(["log", "pty-push", "--raw"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("hello-from-push") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "push input never appeared in PTY log: {stdout}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Python REPL exec works on PTY sessions.
#[test]
fn exec_python_pty() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args([
            "start",
            "py-pty",
            "--stdin",
            "--pty",
            "--exec-target",
            "python-repl",
            "--",
            "python3",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "py-pty");
    harness::wait_running(&root, "py-pty");
    // No sleep: exec buffers the frame and waits for the result file, so the
    // REPL not being input-ready yet is a delay, not a lost command (PR #55).

    let output = tendr(&root)
        .args([
            "exec",
            "py-pty",
            "--timeout",
            "10",
            "--",
            "print('pty hello')",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    assert!(result["stdout"].as_str().unwrap().contains("pty hello"));
}

/// PTY exec is still rejected for shell targets.
#[test]
fn exec_pty_still_rejected_for_shells() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args([
            "start",
            "pty-shell",
            "--stdin",
            "--pty",
            "--exec-target",
            "posix-shell",
            "--",
            "bash",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "pty-shell");
    harness::wait_running(&root, "pty-shell");

    tendr(&root)
        .args(["exec", "pty-shell", "--", "echo", "test"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not supported on PTY"));
}

/// Wait for the attach socket breadcrumb and return the socket path.
fn wait_for_attach_socket(root: &TempDir, session: &str) -> std::path::PathBuf {
    let breadcrumb = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/a.sock.path"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(content) = std::fs::read_to_string(&breadcrumb) {
            let p = std::path::PathBuf::from(content.trim());
            if p.exists() {
                return p;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for attach socket in {session}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Connect, send a v1 hello in `mode`, and return the stream with the sidecar's
/// reply (`(message type, payload)`), read under a deadline.
fn hello(sock_path: &std::path::Path, mode: Mode) -> (UnixStream, (u8, Vec<u8>)) {
    let mut stream = UnixStream::connect(sock_path).expect("failed to connect to attach socket");
    write_msg(&mut stream, MSG_HELLO, &[PROTOCOL_VERSION, mode as u8]);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let reply = read_msg(&mut stream).expect("sidecar replies to hello");
    // Fails with EINVAL on macOS once the sidecar has shut a rejected socket down.
    let _ = stream.set_read_timeout(None);
    (stream, reply)
}

fn epoch_of(payload: &[u8]) -> u64 {
    u64::from_be_bytes(payload.try_into().expect("epoch payload is 8 bytes"))
}

/// Attach as a human through the v1 handshake and hold the connection.
/// Returns the stream so the caller can control when it disconnects.
fn attach_as_human(sock_path: &std::path::Path) -> UnixStream {
    let (stream, (msg_type, payload)) = hello(sock_path, Mode::Attach);
    assert_eq!(
        msg_type,
        MSG_ACCEPTED,
        "attach should be accepted: {}",
        String::from_utf8_lossy(&payload)
    );
    stream
}

/// Poll the PTY output until it contains `needle`, wherever capture split it,
/// and return the output. `tendr log --raw` prints each captured chunk as its
/// own line, so a needle that straddles two reads of the PTY never appears
/// whole there. For a child whose output has no newlines of its own, joining
/// the lines recovers the stream.
fn wait_output_contains(root: &TempDir, session: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = tendr(root)
            .args(["log", session, "--raw"])
            .output()
            .unwrap();
        let joined: String = String::from_utf8_lossy(&output.stdout)
            .chars()
            .filter(|&c| c != '\n')
            .collect();
        if joined.contains(needle) {
            return joined;
        }
        assert!(
            Instant::now() < deadline,
            "output of {session} never contained {needle:?}; got: {joined}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll `tendr log --raw` until it contains `needle`.
fn wait_log_contains(root: &TempDir, session: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = tendr(root)
            .args(["log", session, "--raw"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.contains(needle) {
            return stdout;
        }
        assert!(
            Instant::now() < deadline,
            "log for {session} never contained {needle:?}; got: {stdout}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read until the sidecar closes the stream, returning every message seen.
fn read_until_closed(stream: &mut UnixStream) -> Vec<(u8, Vec<u8>)> {
    // EINVAL on macOS means the socket is already shut down, and a read then
    // returns end of stream immediately, so ignoring it cannot hang.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut seen = Vec::new();
    loop {
        match read_msg(stream) {
            Ok(msg) => seen.push(msg),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                return seen;
            }
            Err(e) => panic!("expected the sidecar to close the stream, got {e}"),
        }
    }
}

/// Wait for meta.json PTY control to reach a specific state.
fn wait_for_pty_control(root: &TempDir, session: &str, expected: &str) {
    let meta_path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(content) = std::fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
                if meta["pty"]["control"].as_str() == Some(expected) {
                    return;
                }
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for pty.control={expected} in {session}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn push_rejected_during_human_control() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start a PTY session with stdin
    tendr(&root)
        .args(["start", "pty-hc", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-hc");
    harness::wait_running(&root, "pty-hc");

    let sock_path = wait_for_attach_socket(&root, "pty-hc");

    // Simulate a human attaching
    let _human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-hc", "HumanControl");

    // Push should be rejected
    let output = tendr(&root)
        .args(["push", "pty-hc"])
        .write_stdin(b"rejected\n")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("human control"),
        "push should be rejected during human control: {stderr}"
    );

    // Drop the human connection (detach)
    drop(_human);
    wait_for_pty_control(&root, "pty-hc", "AgentControl");

    // Push should work again
    let output = tendr(&root)
        .args(["push", "pty-hc"])
        .write_stdin(b"accepted\n")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "push should succeed after detach: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The sidecar's own whole-meta writes must carry the live control owner, not
/// the `AgentControl` it started with. The gated `output_log` fault forces one
/// such write mid-run, after the human has taken control.
#[test]
fn sidecar_meta_write_keeps_human_control() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let gate = root.path().join("fault-gate");

    tendr(&root)
        .env("TENDR_TEST_FAIL", "output_log")
        .env("TENDR_TEST_FAULT_GATE", &gate)
        .args(["start", "pty-keep", "--pty", "--", "cat"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-keep");
    harness::wait_running(&root, "pty-keep");

    let sock_path = wait_for_attach_socket(&root, "pty-keep");
    let _human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-keep", "HumanControl");

    std::fs::write(&gate, b"").unwrap();
    let meta_path = root
        .path()
        .join(".tendr/sessions/default/pty-keep/meta.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    let meta = loop {
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        let written = meta["warnings"].as_array().is_some_and(|w| {
            w.iter()
                .any(|w| w.as_str().is_some_and(|w| w.contains("output.log")))
        });
        if written {
            break meta;
        }
        assert!(
            Instant::now() < deadline,
            "sidecar never rewrote meta: {meta}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    assert_eq!(meta["status"], "Running");
    assert_eq!(meta["pty"]["control"], "HumanControl");
}

#[test]
fn attach_contention_rejected() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-contend", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-contend");
    harness::wait_running(&root, "pty-contend");

    let sock_path = wait_for_attach_socket(&root, "pty-contend");

    // First human attaches
    let _human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-contend", "HumanControl");

    // Second attach via CLI should be rejected
    let output = tendr(&root)
        .args(["attach", "pty-contend"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already under human control"),
        "second attach should be rejected: {stderr}"
    );

    drop(_human);
}

#[test]
fn resize_reaches_child_pty() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // An interactive shell so we can query the child's terminal size post-resize.
    tendr(&root)
        .args(["start", "pty-resize", "--pty", "--stdin", "--", "sh"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-resize");

    let sock_path = wait_for_attach_socket(&root, "pty-resize");
    let mut stream = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-resize", "HumanControl");

    // Collect child output (MSG_DATA) on a reader thread so the observation below
    // is deadline-bounded, not a blocking framed read.
    let seen = Arc::new(Mutex::new(String::new()));
    let reader = {
        let mut r = stream.try_clone().unwrap();
        let seen = Arc::clone(&seen);
        std::thread::spawn(move || {
            while let Ok((msg_type, payload)) = read_msg(&mut r) {
                if msg_type == MSG_DATA {
                    seen.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_str(&String::from_utf8_lossy(&payload));
                }
            }
        })
    };

    // 1) Resize to 40x120. 2) On the SAME ordered socket, ask the child its
    // terminal size. The listener processes messages sequentially, so `stty size`
    // runs only after apply_pty_resize returns — child-reported "40 120" proves
    // the resize actually reached the child's PTY, not merely that it parsed.
    //
    // The sentinel is split as `__RESIZE""_DONE__` to make it a *causal*
    // completion token: the shell's echo of the input line cannot contain the
    // bare `__RESIZE_DONE__` (the quotes are only stripped when the command
    // runs), so that token can appear only in the command's output — strictly
    // after `stty size` has printed the dimensions.
    write_msg(
        &mut stream,
        MSG_RESIZE,
        &resize_payload(Geometry::new(40, 120).unwrap()),
    );
    write_msg(
        &mut stream,
        MSG_DATA,
        b"stty size; echo __RESIZE\"\"_DONE__\n",
    );

    // Observe by *returning a result*, not asserting — so a timeout cannot bypass
    // the detach/join/kill cleanup below and leak the PTY session. Require BOTH
    // the child-observed dimensions (load-bearing) and the completion token
    // (proves `stty size` finished, so the dimensions are settled, not mid-write).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let observed: Result<(), String> = loop {
        let out = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if out.contains("40 120") && out.contains("__RESIZE_DONE__") {
            break Ok(());
        }
        if std::time::Instant::now() >= deadline {
            break Err(format!(
                "child never reported the resized dimensions + completion token; got: {out:?}"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };

    // Secondary safety snapshot, taken while still under human control.
    let status = tendr(&root)
        .args(["status", "pty-resize"])
        .output()
        .unwrap();
    let meta: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&status.stdout)).unwrap();

    // Deterministic cleanup that runs regardless of the observation outcome:
    // MSG_DETACH makes the listener close the connection, so the reader receives
    // EOF and joins without depending on the kill; then the shell is reaped.
    write_msg(&mut stream, MSG_DETACH, &[]);
    drop(stream);
    let _ = reader.join();
    tendr(&root)
        .args(["kill", "pty-resize", "--force"])
        .assert()
        .success();

    // Assert only after cleanup has run.
    observed.expect("resize observation");
    assert_eq!(meta["status"], "Running", "session should still be running");
    assert_eq!(meta["pty"]["control"], "HumanControl");
}

// --- Slice 3: pty.control_changed events (plan scope 6) ---

/// Attach then detach emits two pty.control_changed events — the shipped
/// PtyControl vocabulary, exactly the two pinned data fields, appended
/// before the corresponding meta flip, from the attach thread's own writer.
#[test]
fn attach_detach_emit_control_changed_events() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-ev", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-ev");
    harness::wait_running(&root, "pty-ev");
    let sock_path = wait_for_attach_socket(&root, "pty-ev");

    let human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-ev", "HumanControl");
    // WAL order: once meta shows the flip, the event must already be stored.
    let events = harness::read_events(&root, "pty-ev");
    assert!(
        events.iter().any(|e| e["kind"] == "pty.control_changed"),
        "event lands before the meta control write"
    );

    drop(human);
    wait_for_pty_control(&root, "pty-ev", "AgentControl");

    let events = harness::read_events(&root, "pty-ev");
    let changed: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "pty.control_changed")
        .collect();
    assert_eq!(changed.len(), 2);
    assert_eq!(
        changed[0]["data"],
        serde_json::json!({"control": "HumanControl", "trigger": "attach"}),
        "minimal by design — a control fact, not a screen event"
    );
    assert_eq!(
        changed[1]["data"],
        serde_json::json!({"control": "AgentControl", "trigger": "detach"})
    );
    for event in &changed {
        assert_eq!(event["source"], "tendr.sidecar");
    }

    // The attach thread owns its own writer (multi-writer by design).
    let started = events.iter().find(|e| e["kind"] == "run.started").unwrap();
    assert_ne!(
        changed[0]["writer"], started["writer"],
        "not the lifecycle writer"
    );
    assert_eq!(changed[0]["writer"], changed[1]["writer"]);
    assert_eq!(changed[0]["seq"], 1);
    assert_eq!(changed[1]["seq"], 2);
}

// --- Cloud PTY control, slice 1: sidecar-enforced input authority ---

/// Run the real `tendr attach` CLI inside a PTY, as a terminal user would.
struct CliAttach {
    child: <tendr::platform::Current as tendr::platform::Platform>::SupervisedChild,
    input: Option<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    /// While set, the terminal stops reading the CLI's output, as a stalled
    /// terminal or SSH connection would.
    output_paused: Arc<std::sync::atomic::AtomicBool>,
    resize_fd: Option<std::fs::File>,
}

impl CliAttach {
    fn spawn(root: &TempDir, args: &[&str]) -> Self {
        let mut argv = vec![tendr_bin(), "attach".to_owned()];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        Self::spawn_argv(root, &argv)
    }

    fn spawn_argv(root: &TempDir, argv: &[String]) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tendr::platform::{Current, Platform};
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "HOME".to_owned(),
            root.path().to_string_lossy().into_owned(),
        );
        let mut child = Current::spawn_child_pty(argv, None, &env).unwrap();
        // Take the resize handle before stdin takes the master's write half.
        let resize_fd = Current::pty_resize_fd(&child);
        let input = Current::child_stdin(&mut child).unwrap();
        let mut reader = Current::child_stdout(&mut child).unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let output_paused = Arc::new(AtomicBool::new(false));
        let (sink, paused) = (Arc::clone(&output), Arc::clone(&output_paused));
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                if paused.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                match std::io::Read::read(&mut reader, &mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
        });
        Self {
            child,
            input: Some(input),
            output,
            output_paused,
            resize_fd,
        }
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.input
            .as_mut()
            .expect("input still owned")
            .write_all(bytes)
            .unwrap();
    }

    /// Hand the terminal's keyboard to another thread (for writes that block).
    fn take_input(&mut self) -> Box<dyn Write + Send> {
        self.input.take().expect("input still owned")
    }

    fn pause_output(&self) {
        self.output_paused
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn resume_output(&self) {
        self.output_paused
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Resize the CLI's terminal, as a user resizing the Ghostty window would.
    fn resize(&self, rows: u16, cols: u16) {
        use std::os::unix::io::AsRawFd;
        let fd = self.resize_fd.as_ref().expect("resize handle").as_raw_fd();
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `fd` is an open PTY master; TIOCSWINSZ takes a winsize pointer.
        let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
        assert_eq!(rc, 0, "resize the CLI's terminal");
    }

    /// Type `line` until the session log shows it. Keystrokes typed before the CLI
    /// enters raw mode are discarded (`TCSAFLUSH`), so a single write races the
    /// handshake; the marker is idempotent, so retyping is safe.
    fn type_until_logged(&mut self, root: &TempDir, session: &str, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.type_bytes(line.as_bytes());
            let output = tendr(root)
                .args(["log", session, "--raw"])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&output.stdout).contains(line.trim_end()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "typed {line:?} never reached {session}; cli output: {}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }
}

impl Drop for CliAttach {
    fn drop(&mut self) {
        use tendr::platform::{Current, Platform};
        self.resume_output();
        let kill = Current::child_kill_handle(&self.child);
        let _ = Current::kill_child(&kill, true);
        let _ = Current::child_wait(&mut self.child);
    }
}

fn tendr_bin() -> String {
    assert_cmd::cargo::cargo_bin("tendr")
        .to_string_lossy()
        .into_owned()
}

/// `tendr attach` run by a shell in the CLI's terminal, recording the terminal
/// settings (`stty -g`) before and after it and its exit code, so tests can
/// prove the terminal was restored however the attach ended.
struct WrappedAttach {
    cli: CliAttach,
    before: std::path::PathBuf,
    after: std::path::PathBuf,
    code: std::path::PathBuf,
}

impl WrappedAttach {
    fn spawn(root: &TempDir, label: &str, args: &[&str]) -> Self {
        let dir = root.path().join(format!("wrap-{label}"));
        std::fs::create_dir_all(&dir).unwrap();
        let q = |p: &std::path::Path| shell_words::quote(p.to_str().unwrap()).into_owned();
        let (before, after, code) = (dir.join("before"), dir.join("after"), dir.join("code"));
        let attach: Vec<String> = std::iter::once(tendr_bin())
            .chain(std::iter::once("attach".to_owned()))
            .chain(args.iter().map(|a| (*a).to_owned()))
            .map(|a| shell_words::quote(&a).into_owned())
            .collect();
        let script = format!(
            "stty -g > {b}.tmp && mv {b}.tmp {b}; {cmd}; echo $? > {c}.tmp; \
             stty -g > {a}.tmp; mv {a}.tmp {a}; mv {c}.tmp {c}; sleep 60",
            b = q(&before),
            a = q(&after),
            c = q(&code),
            cmd = attach.join(" "),
        );
        let cli = CliAttach::spawn_argv(root, &["sh".to_owned(), "-c".to_owned(), script]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !before.exists() {
            assert!(Instant::now() < deadline, "wrapper never started");
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            cli,
            before,
            after,
            code,
        }
    }

    fn exited(&self) -> bool {
        self.code.exists()
    }

    /// Wait for `tendr attach` to exit and return its exit code.
    fn wait_exit(&self, within: Duration) -> i32 {
        let deadline = Instant::now() + within;
        while !self.code.exists() {
            assert!(
                Instant::now() < deadline,
                "tendr attach did not exit within {within:?}; cli output: {}",
                self.cli.output()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::read_to_string(&self.code)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn assert_terminal_restored(&self) {
        let before = std::fs::read_to_string(&self.before).unwrap();
        let after = std::fs::read_to_string(&self.after).unwrap();
        assert_eq!(
            before, after,
            "terminal settings after attach differ from before"
        );
    }
}

#[test]
fn cli_escape_detaches_and_restores_the_terminal() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-escape");
    start_cat(&root, "pty-escape");

    let mut wrapped = WrappedAttach::spawn(&root, "escape", &["pty-escape"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-escape", "before-detach\n");
    wrapped.cli.type_bytes(b"\x1cd");

    assert_eq!(wrapped.wait_exit(Duration::from_secs(10)), 0);
    wrapped.assert_terminal_restored();
    wait_for_pty_control(&root, "pty-escape", "AgentControl");
    push(&root, "pty-escape", b"session-still-running\n");
    wait_log_contains(&root, "pty-escape", "session-still-running");
}

/// A PTY child that shows control characters: `Ctrl-\` prints as `^\`.
fn start_visible_cat(root: &TempDir, session: &str) {
    tendr(root)
        .args(["start", session, "--pty", "--stdin", "--"])
        .args(["sh", "-c", "stty raw -echo; exec cat -v"])
        .output()
        .unwrap();
    harness::wait_running(root, session);
}

#[test]
fn cli_doubled_escape_and_other_keys_reach_the_session_unchanged() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-literal");
    start_visible_cat(&root, "pty-literal");

    let mut wrapped = WrappedAttach::spawn(&root, "literal", &["pty-literal"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-literal", "literal-ready\n");
    // Doubled prefix → one literal; prefix + other key → both; plain keys as typed.
    wrapped.cli.type_bytes(b"A\x1c\x1cB\x1cxC\n");
    wait_log_contains(&root, "pty-literal", "A^\\B^\\xC");
    assert!(!wrapped.exited(), "no detach was requested");
}

#[test]
fn cli_escape_none_forwards_the_detach_sequence() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-noescape");
    start_visible_cat(&root, "pty-noescape");

    let mut wrapped =
        WrappedAttach::spawn(&root, "noescape", &["pty-noescape", "--escape", "none"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-noescape", "none-ready\n");
    wrapped.cli.type_bytes(b"N\x1cdM\n");
    wait_log_contains(&root, "pty-noescape", "N^\\dM");
    assert!(!wrapped.exited(), "--escape none must not detach");
}

#[test]
fn cli_forwards_later_terminal_resizes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-winch");
    tendr(&root)
        .args(["start", "pty-winch", "--pty", "--stdin", "--", "sh"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-winch");

    let mut cli = CliAttach::spawn(&root, &["pty-winch"]);
    cli.type_until_logged(&root, "pty-winch", "echo winch-ready\n");

    // Resized after the attach started: only continuous forwarding delivers it.
    cli.resize(33, 111);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cli.type_bytes(b"stty size\n");
        let log = tendr(&root)
            .args(["log", "pty-winch", "--raw"])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&log.stdout).contains("33 111") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the session never saw the new size; cli output: {}",
            cli.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn cli_takeover_by_another_client_restores_the_terminal() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-taken");
    let sock_path = start_cat(&root, "pty-taken");

    let mut wrapped = WrappedAttach::spawn(&root, "taken", &["pty-taken"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-taken", "before-takeover\n");
    let (_other, (msg_type, _)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);

    wrapped.wait_exit(Duration::from_secs(10));
    wrapped.assert_terminal_restored();
    assert!(
        wrapped.cli.output().contains("took over"),
        "the user is told why the attach ended: {}",
        wrapped.cli.output()
    );
}

#[test]
fn cli_session_end_restores_the_terminal() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-ended");
    start_cat(&root, "pty-ended");

    let mut wrapped = WrappedAttach::spawn(&root, "ended", &["pty-ended"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-ended", "before-end\n");
    tendr(&root)
        .args(["kill", "pty-ended", "--force"])
        .output()
        .unwrap();

    wrapped.wait_exit(Duration::from_secs(10));
    wrapped.assert_terminal_restored();
}

#[test]
fn cli_detach_is_responsive_while_session_input_is_blocked() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-inblock");
    // The child never reads its input, so everything typed backs up.
    tendr(&root)
        .args(["start", "pty-inblock", "--pty", "--stdin", "--"])
        .args([
            "sh",
            "-c",
            "stty raw -echo; while true; do printf READY; sleep 0.2; done",
        ])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-inblock");

    let mut wrapped = WrappedAttach::spawn(&root, "inblock", &["pty-inblock"]);
    // Relayed output proves the CLI is in raw mode, so Ctrl-\ is not SIGQUIT.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !wrapped.cli.output().contains("READY") {
        assert!(Instant::now() < deadline, "attach never relayed output");
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut keyboard = wrapped.cli.take_input();
    let typist = std::thread::spawn(move || {
        let _ = keyboard.write_all(&vec![b'x'; 256 * 1024]);
        let _ = keyboard.write_all(b"\x1cd");
        keyboard
    });

    assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 0);
    wrapped.assert_terminal_restored();
    wait_for_pty_control(&root, "pty-inblock", "AgentControl");
    drop(typist.join());
}

#[test]
fn cli_detach_is_responsive_while_terminal_output_is_blocked() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-outblock");
    tendr(&root)
        .args(["start", "pty-outblock", "--pty", "--stdin", "--"])
        .args(["sh", "-c", "stty raw -echo; yes OUTPUT-FLOOD"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-outblock");

    let mut wrapped = WrappedAttach::spawn(&root, "outblock", &["pty-outblock"]);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !wrapped.cli.output().contains("OUTPUT-FLOOD") {
        assert!(Instant::now() < deadline, "attach never relayed output");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The terminal stops reading; the flood fills its output buffer and the
    // CLI's writes to it block. That state cannot be observed directly, so give
    // the flood a moment to fill it.
    wrapped.cli.pause_output();
    std::thread::sleep(Duration::from_millis(500));
    wrapped.cli.type_bytes(b"\x1cd");

    let code = wrapped.wait_exit(Duration::from_secs(15));
    wrapped.cli.resume_output();
    assert_eq!(code, 0);
    wrapped.assert_terminal_restored();
}

#[test]
fn cli_attach_delivers_typed_input_through_the_handshake() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-cli");
    start_cat(&root, "pty-cli");

    let mut cli = CliAttach::spawn(&root, &["pty-cli"]);
    wait_for_pty_control(&root, "pty-cli", "HumanControl");
    cli.type_until_logged(&root, "pty-cli", "typed-through-cli\n");
    assert!(!cli.output().contains("error"), "cli: {}", cli.output());
}

#[test]
fn cli_attach_takeover_retires_the_current_controller() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-cli-take");
    let sock_path = start_cat(&root, "pty-cli-take");
    let mut old = attach_as_human(&sock_path);

    let mut cli = CliAttach::spawn(&root, &["pty-cli-take", "--takeover"]);
    let seen = read_until_closed(&mut old);
    assert!(
        seen.iter().any(|(t, _)| *t == MSG_RETIRED),
        "the raw controller is retired by the CLI takeover: {seen:?}; cli: {}",
        cli.output()
    );
    cli.type_until_logged(&root, "pty-cli-take", "cli-took-over\n");
}

/// Start `cat` under a PTY with `--stdin` and return its attach socket.
fn start_cat(root: &TempDir, session: &str) -> std::path::PathBuf {
    tendr(root)
        .args(["start", session, "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    harness::wait_running(root, session);
    wait_for_attach_socket(root, session)
}

fn push(root: &TempDir, session: &str, bytes: &[u8]) {
    let output = tendr(root)
        .args(["push", session])
        .write_stdin(bytes.to_vec())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn connection_without_hello_gains_no_control_and_is_closed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-nohello");
    let sock_path = start_cat(&root, "pty-nohello");

    let mut raw = UnixStream::connect(&sock_path).unwrap();
    write_msg(&mut raw, MSG_DATA, b"no-hello-marker\n");
    let seen = read_until_closed(&mut raw);
    assert!(
        seen.iter().all(|(t, _)| *t != MSG_ACCEPTED),
        "a connection without hello must never be accepted: {seen:?}"
    );

    // Causal barrier: input that arrives later through push is visible, so the
    // earlier marker would be visible too had it reached the PTY.
    push(&root, "pty-nohello", b"control-marker\n");
    let log = wait_log_contains(&root, "pty-nohello", "control-marker");
    assert!(!log.contains("no-hello-marker"), "log: {log}");

    let status = tendr(&root)
        .args(["status", "pty-nohello"])
        .output()
        .unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(meta["pty"]["control"], "AgentControl");
}

#[test]
fn attach_is_accepted_with_an_epoch_and_a_second_attach_is_rejected() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-epoch");
    let sock_path = start_cat(&root, "pty-epoch");

    let (mut first, (msg_type, payload)) = hello(&sock_path, Mode::Attach);
    assert_eq!(msg_type, MSG_ACCEPTED);
    assert_eq!(epoch_of(&payload), 1);
    wait_for_pty_control(&root, "pty-epoch", "HumanControl");

    let (mut second, (msg_type, payload)) = hello(&sock_path, Mode::Attach);
    assert_eq!(
        msg_type,
        MSG_REJECTED,
        "a plain attach must not supersede: {}",
        String::from_utf8_lossy(&payload)
    );
    let _ = read_until_closed(&mut second);

    // The first controller is undisturbed.
    write_msg(&mut first, MSG_DATA, b"first-still-owns\n");
    wait_log_contains(&root, "pty-epoch", "first-still-owns");

    drop(first);
}

#[test]
fn takeover_retires_the_previous_human_and_rejects_its_later_input() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-takeover");
    let sock_path = start_cat(&root, "pty-takeover");

    let mut old = attach_as_human(&sock_path);
    let (mut new, (msg_type, payload)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);
    assert_eq!(epoch_of(&payload), 2);

    let seen = read_until_closed(&mut old);
    let retired: Vec<_> = seen.iter().filter(|(t, _)| *t == MSG_RETIRED).collect();
    assert_eq!(retired.len(), 1, "old controller is told it was retired");
    assert_eq!(epoch_of(&retired[0].1), 2);

    // The retired connection is shut down; anything it still manages to send is
    // rejected. Its write may fail outright, which is equally correct.
    let _ = old.write_all(&[MSG_DATA, 0, 0, 0, 13]);
    let _ = old.write_all(b"old-after-rt\n");

    write_msg(&mut new, MSG_DATA, b"new-owns-now\n");
    let log = wait_log_contains(&root, "pty-takeover", "new-owns-now");
    assert!(!log.contains("old-after-rt"), "log: {log}");

    let takeover_event = harness::read_events(&root, "pty-takeover")
        .into_iter()
        .find(|e| e["kind"] == "pty.control_changed" && e["data"]["trigger"] == "takeover");
    assert!(takeover_event.is_some(), "takeover is recorded as a fact");

    drop(new);
}

#[test]
fn takeover_revokes_queued_agent_input() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-revoke");
    let go = root.path().join("go");

    // The child consumes exactly 1 KiB of agent input, reports READY, then stops
    // reading until the go file exists. Everything the agent pushes after that
    // stays queued behind a full PTY input buffer.
    let script = "stty raw -echo; head -c 1024 >/dev/null; printf READY; \
                  while [ ! -f \"$GO\" ]; do sleep 0.05; done; exec cat";
    tendr(&root)
        .args(["start", "pty-revoke", "--pty", "--stdin"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-revoke");
    let sock_path = wait_for_attach_socket(&root, "pty-revoke");

    let mut payload = vec![b'a'; 2 << 20];
    payload.extend_from_slice(b"AGENT-TAIL\n");
    let mut agent = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["push", "pty-revoke"])
        .env("HOME", root.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut agent_stdin = agent.stdin.take().unwrap();
    let feeder = std::thread::spawn(move || {
        // The write fails with a broken pipe if the sidecar stops reading; either
        // outcome is fine for the feeder.
        let _ = agent_stdin.write_all(&payload);
    });

    // Barrier: agent input is flowing and the rest of it is queued.
    wait_log_contains(&root, "pty-revoke", "READY");
    assert!(
        agent.try_wait().unwrap().is_none(),
        "setup invariant: the agent push must still be in flight at takeover"
    );

    let (mut human, (msg_type, _)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);
    write_msg(&mut human, MSG_DATA, b"HUMAN-LINE\n");
    std::fs::write(&go, b"").unwrap();

    // The human's line can reach the PTY in pieces as the child drains the
    // agent's buffered prefix, so it may straddle two captured chunks.
    let log = wait_output_contains(&root, "pty-revoke", "HUMAN-LINE");
    assert!(
        !log.contains("AGENT-TAIL"),
        "queued agent input was written after takeover"
    );
    let a_count = log.bytes().filter(|&b| b == b'a').count();
    assert!(
        a_count < 1 << 20,
        "at most the kernel-buffered prefix may precede the takeover; saw {a_count} bytes"
    );
    let revoked = harness::wait_event_kind(&root, "pty-revoke", "pty.input_revoked");
    assert_eq!(revoked["data"]["kind"], "Agent");

    // The revoked push itself reports it: no false success.
    drop(human);
    feeder.join().unwrap();
    let pushed = agent.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&pushed.stderr);
    assert!(
        !pushed.status.success(),
        "a push revoked by takeover must not exit 0; stderr: {stderr}"
    );
    assert!(
        stderr.contains("revoked"),
        "the failure says the push was revoked: {stderr}"
    );
    tendr(&root)
        .args(["kill", "pty-revoke", "--force"])
        .output()
        .ok();
}

#[test]
fn takeover_after_a_silent_connection_reaches_the_same_running_process() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-reconnect");

    tendr(&root)
        .args(["start", "pty-reconnect", "--pty", "--stdin", "--", "sh"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-reconnect");
    let sock_path = wait_for_attach_socket(&root, "pty-reconnect");
    let status = tendr(&root)
        .args(["status", "pty-reconnect"])
        .output()
        .unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let child_pid = meta["child"]["pid"].as_u64().unwrap();

    // A controller that goes silent without closing, like a half-open SSH session.
    let _silent = attach_as_human(&sock_path);

    let (mut new, (msg_type, _)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);
    write_msg(&mut new, MSG_DATA, b"echo \"pid=\"$$\n");

    let expected = format!("pid={child_pid}");
    wait_log_contains(&root, "pty-reconnect", &expected);

    drop(new);
    tendr(&root)
        .args(["kill", "pty-reconnect", "--force"])
        .output()
        .ok();
}

#[test]
fn stalled_viewer_cannot_block_output_capture() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-stalled");
    let go = root.path().join("go");

    // After the go file appears, the child writes ~10 MiB — more than one
    // viewer's queue budget — then a marker, then stays alive.
    let script = "while [ ! -f \"$GO\" ]; do sleep 0.05; done; \
                  yes 0123456789abcdef0123456789abcdef | head -c 10485760; \
                  echo; echo ALL-OUTPUT-DONE; sleep 60";
    tendr(&root)
        .args(["start", "pty-stalled", "--pty"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-stalled");
    let sock_path = wait_for_attach_socket(&root, "pty-stalled");

    // A viewer that attaches and then never reads.
    let mut stalled = attach_as_human(&sock_path);
    std::fs::write(&go, b"").unwrap();

    // Capture must keep up regardless of the stalled viewer. Read output.log
    // directly: `tendr log` would re-parse ~10 MiB on every poll.
    let log_path = root
        .path()
        .join(".tendr/sessions/default/pty-stalled/output.log");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let log = std::fs::read(&log_path).unwrap_or_default();
        if log.windows(15).any(|w| w == b"ALL-OUTPUT-DONE") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "output capture stalled behind a viewer that never reads ({} log bytes)",
            log.len()
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // The stalled viewer was disconnected rather than buffered without bound,
    // and with it went its control.
    let _ = read_until_closed(&mut stalled);
    wait_for_pty_control(&root, "pty-stalled", "AgentControl");
}

#[test]
fn a_second_push_is_refused_while_one_holds_the_terminal() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-twopush");
    let go = root.path().join("go");
    // The child consumes exactly 1 KiB of the first push, reports READY, then
    // reads nothing until the go file exists: the first push stays in flight.
    let script = "stty raw -echo; head -c 1024 >/dev/null; printf READY; \
                  while [ ! -f \"$GO\" ]; do sleep 0.05; done; exec cat";
    tendr(&root)
        .args(["start", "pty-twopush", "--pty", "--stdin"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-twopush");

    let mut first = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["push", "pty-twopush"])
        .env("HOME", root.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut first_stdin = first.stdin.take().unwrap();
    let feeder = std::thread::spawn(move || {
        let _ = first_stdin.write_all(&vec![b'a'; 1 << 20]);
    });
    // Barrier: the first push holds the terminal and its input is flowing.
    wait_log_contains(&root, "pty-twopush", "READY");

    let second = tendr(&root)
        .args(["push", "pty-twopush"])
        .write_stdin(b"SECOND-PUSH\n".to_vec())
        .timeout(Duration::from_secs(20))
        .output()
        .expect("a second push must return promptly, not block behind the first");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        !second.status.success(),
        "a concurrent push must be refused, not interleaved: {stderr}"
    );
    assert!(
        stderr.contains("owned"),
        "the refusal names the owner: {stderr}"
    );

    std::fs::write(&go, b"").unwrap();
    feeder.join().unwrap();
    let first = first.wait_with_output().unwrap();
    assert!(
        first.status.success(),
        "the first push completes: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let log = wait_log_contains(&root, "pty-twopush", "aaaa");
    assert!(!log.contains("SECOND-PUSH"), "refused bytes were written");
}

#[test]
fn attach_socket_is_private_and_under_the_state_root() {
    use std::os::unix::fs::PermissionsExt;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-private");
    let sock_path = start_cat(&root, "pty-private");

    let sockets = root.path().join(".tendr").join("sockets");
    assert_eq!(
        sock_path
            .parent()
            .map(std::fs::canonicalize)
            .transpose()
            .unwrap(),
        Some(std::fs::canonicalize(&sockets).unwrap()),
        "socket lives in the state root's private directory, not shared temp"
    );
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&sockets), 0o700);
    assert_eq!(mode(&sock_path), 0o600);
}

#[test]
fn oversized_attach_frame_closes_the_connection_and_releases_control() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-bigframe");
    let sock_path = start_cat(&root, "pty-bigframe");

    let mut human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-bigframe", "HumanControl");
    // Declare a 1 GiB payload; the sidecar must not wait for or allocate it.
    human
        .write_all(&[MSG_DATA, 0x40, 0x00, 0x00, 0x00])
        .unwrap();
    let _ = read_until_closed(&mut human);
    wait_for_pty_control(&root, "pty-bigframe", "AgentControl");

    // The session is unaffected.
    push(&root, "pty-bigframe", b"still-running\n");
    wait_log_contains(&root, "pty-bigframe", "still-running");
}

#[test]
fn a_trickled_hello_is_cut_off_at_the_overall_deadline() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-slowhello");
    let sock_path = start_cat(&root, "pty-slowhello");

    // A well-formed hello sent one byte per second: every individual read is
    // quick, but the whole handshake takes 7 s, past the 5 s deadline.
    let mut slow = UnixStream::connect(&sock_path).unwrap();
    let hello = [MSG_HELLO, 0, 0, 0, 2, PROTOCOL_VERSION, Mode::Attach as u8];
    let started = Instant::now();
    for byte in hello {
        if slow.write_all(&[byte]).is_err() {
            break; // already closed by the sidecar
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let seen = read_until_closed(&mut slow);
    assert!(
        seen.iter().all(|(t, _)| *t != MSG_ACCEPTED),
        "a hello past its deadline must not be accepted: {seen:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "closed promptly after the deadline"
    );
    let status = tendr(&root)
        .args(["status", "pty-slowhello"])
        .output()
        .unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(meta["pty"]["control"], "AgentControl");
}

#[test]
fn an_unsafe_socket_directory_fails_start_loudly() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-unsafe");
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::create_dir_all(root.path().join(".tendr")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.path().join(".tendr/sockets")).unwrap();

    let output = tendr(&root)
        .args(["start", "pty-unsafe", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    let meta = harness::wait_terminal(&root, "pty-unsafe");
    assert_eq!(
        meta["status"],
        "SpawnFailed",
        "a PTY session must not run without its private listener; start said: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let warnings = meta["warnings"].to_string();
    assert!(
        warnings.contains("socket"),
        "the failure names the socket: {warnings}"
    );
    assert!(
        std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
        "nothing was created through the symlink"
    );
}

#[test]
fn detach_releases_control_even_when_pty_input_is_full() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-full-detach");
    // The child never reads its input, so the PTY input buffer fills.
    tendr(&root)
        .args(["start", "pty-full-detach", "--pty", "--stdin", "--"])
        .args(["sh", "-c", "stty raw -echo; printf READY; sleep 60"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-full-detach");
    wait_log_contains(&root, "pty-full-detach", "READY");
    let sock_path = wait_for_attach_socket(&root, "pty-full-detach");

    let mut human = attach_as_human(&sock_path);
    human
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    for _ in 0..16 {
        write_msg(&mut human, MSG_DATA, &vec![b'x'; 65536]);
    }
    write_msg(&mut human, MSG_DETACH, &[]);
    let _ = read_until_closed(&mut human);

    // The detached client must not keep control behind its unwritable input.
    wait_for_pty_control(&root, "pty-full-detach", "AgentControl");
    let (_next, (msg_type, reason)) = hello(&sock_path, Mode::Attach);
    assert_eq!(
        msg_type,
        MSG_ACCEPTED,
        "a detached client still holds control: {}",
        String::from_utf8_lossy(&reason)
    );
}

#[test]
fn a_disconnected_push_releases_control_behind_a_full_pty() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-push-gone");
    tendr(&root)
        .args(["start", "pty-push-gone", "--pty", "--stdin", "--"])
        .args([
            "sh",
            "-c",
            "stty raw -echo; head -c 1024 >/dev/null; printf READY; sleep 60",
        ])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-push-gone");
    let sock_path = wait_for_attach_socket(&root, "pty-push-gone");

    // A push client that sends more than the child will read, then vanishes
    // while the sidecar is still waiting for the PTY to accept it.
    let (mut agent, (msg_type, _)) = hello(&sock_path, Mode::Push);
    assert_eq!(msg_type, MSG_ACCEPTED);
    agent
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write_msg(&mut agent, MSG_DATA, &vec![b'x'; 65536]);
    wait_log_contains(&root, "pty-push-gone", "READY");
    agent.shutdown(std::net::Shutdown::Both).unwrap();
    drop(agent);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_next, (msg_type, reason)) = hello(&sock_path, Mode::Attach);
        if msg_type == MSG_ACCEPTED {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a disconnected push still holds the terminal: {}",
            String::from_utf8_lossy(&reason)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn idle_pushes_retired_by_takeover_do_not_exhaust_connection_slots() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-push-slots");
    let sock_path = start_cat(&root, "pty-push-slots");

    // More rounds than there are connection slots. Each push stays connected and
    // idle; the client never closes it.
    let mut idle_pushes = Vec::new();
    for round in 0..9 {
        let (mut push, (msg_type, reason)) = hello(&sock_path, Mode::Push);
        assert_eq!(
            msg_type,
            MSG_ACCEPTED,
            "push {round}: {}",
            String::from_utf8_lossy(&reason)
        );
        let (mut human, (msg_type, reason)) = hello(&sock_path, Mode::Takeover);
        assert_eq!(
            msg_type,
            MSG_ACCEPTED,
            "takeover {round}: {}",
            String::from_utf8_lossy(&reason)
        );

        // The retired push is told, and its connection is closed server-side.
        let seen = read_until_closed(&mut push);
        let done = seen
            .iter()
            .find(|(t, _)| *t == MSG_INPUT_DONE)
            .unwrap_or_else(|| panic!("push {round} got no outcome: {seen:?}"));
        assert!(
            matches!(
                tendr::attach_proto::Frame::decode(done.0, done.1.clone()),
                Ok(tendr::attach_proto::Frame::InputDone(
                    tendr::attach_proto::PushOutcome::Revoked(_)
                ))
            ),
            "push {round}: {done:?}"
        );
        idle_pushes.push(push);

        write_msg(&mut human, MSG_DETACH, &[]);
        let _ = read_until_closed(&mut human);
        wait_for_pty_control(&root, "pty-push-slots", "AgentControl");
    }
}

#[test]
fn an_old_runs_cleanup_cannot_remove_its_replacements_breadcrumb() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-replaced");
    let callback = root.path().join("callback.sh");
    let started = root.path().join("callback-started");
    let finish = root.path().join("callback-finish");
    std::fs::write(
        &callback,
        b"touch \"$1\"; i=0; while [ ! -f \"$2\" ] && [ \"$i\" -lt 400 ]; do sleep 0.05; i=$((i+1)); done\n",
    )
    .unwrap();
    let on_exit = format!(
        "sh {} {} {}",
        callback.display(),
        started.display(),
        finish.display()
    );
    let first = tendr(&root)
        .args(["start", "pty-replaced", "--pty", "--stdin", "--on-exit"])
        .arg(&on_exit)
        .args(["--", "cat"])
        .output()
        .unwrap();
    let old_run: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let old_run_id = old_run["run_id"].as_str().unwrap().to_owned();
    harness::wait_running(&root, "pty-replaced");
    let old_socket = wait_for_attach_socket(&root, "pty-replaced");

    // End the old run; its exit callback runs after the session lock is released.
    tendr(&root)
        .args(["kill", "pty-replaced", "--force"])
        .output()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.exists() {
        assert!(Instant::now() < deadline, "exit callback never started");
        std::thread::sleep(Duration::from_millis(20));
    }

    // A replacement starts while the old sidecar is still in its callback.
    tendr(&root)
        .args([
            "start",
            "pty-replaced",
            "--replace",
            "--pty",
            "--stdin",
            "--",
            "cat",
        ])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-replaced");
    let new_socket = wait_for_attach_socket(&root, "pty-replaced");
    assert_ne!(old_socket, new_socket);

    // Let the old sidecar finish: its callback record is written afterwards.
    std::fs::write(&finish, b"").unwrap();
    let record = root
        .path()
        .join(format!(".tendr/callbacks/{old_run_id}.json"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !record.exists() {
        assert!(Instant::now() < deadline, "old sidecar never finished");
        std::thread::sleep(Duration::from_millis(20));
    }

    let session = root.path().join(".tendr/sessions/default/pty-replaced");
    assert!(!old_socket.exists(), "the old run's socket is gone");
    assert_eq!(
        tendr::attach_proto::read_sock_path(&session),
        Some(new_socket),
        "the old run's cleanup removed the replacement's breadcrumb"
    );
}

// --- Cloud PTY control, slice 1: exact recording ---

fn session_meta(root: &TempDir, session: &str) -> serde_json::Value {
    let path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The run's recording directory, as named by its metadata.
fn recording_dir(root: &TempDir, session: &str) -> std::path::PathBuf {
    let meta = session_meta(root, session);
    let dir = meta["pty"]["recording"]["dir"]
        .as_str()
        .unwrap_or_else(|| panic!("no recording in metadata: {meta}"));
    assert_eq!(
        dir,
        format!("recording/{}", meta["run_id"].as_str().unwrap()),
        "one directory per run"
    );
    root.path()
        .join(format!(".tendr/sessions/default/{session}"))
        .join(dir)
}

fn decode_session_recording(root: &TempDir, session: &str) -> tendr::recording::DecodedRecording {
    let dir = recording_dir(root, session);
    let files = tendr::recorder::DirectoryStore::segments(&dir).unwrap();
    let bytes: Vec<Vec<u8>> = files.iter().map(|f| std::fs::read(f).unwrap()).collect();
    tendr::recording::decode_recording(bytes.iter().map(Vec::as_slice)).expect("recording decodes")
}

fn recorded_output(records: &[tendr::recording::Record]) -> Vec<u8> {
    records
        .iter()
        .filter_map(|r| match &r.kind {
            tendr::recording::RecordKind::Output(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn pty_output_is_recorded_exactly_with_the_initial_geometry() {
    use std::os::unix::fs::PermissionsExt;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Escape sequences, bytes that are not UTF-8, and no trailing newline.
    tendr(&root)
        .args([
            "start",
            "pty-rec",
            "--pty",
            "--",
            "sh",
            "-c",
            "stty size; printf '\\033[31mred\\377\\376tail'",
        ])
        .output()
        .unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-rec");
    harness::wait_terminal(&root, "pty-rec");

    let recording = decode_session_recording(&root, "pty-rec");
    assert_eq!(
        recorded_output(&recording.records),
        b"24 80\r\n\x1b[31mred\xff\xfetail",
        "the child starts at 24x80 and every output byte is recorded as written"
    );
    assert_eq!(recording.end, tendr::recording::SegmentEnd::Clean);

    let meta = session_meta(&root, "pty-rec");
    let header = &recording.header;
    assert_eq!(
        header.run_id.as_uuid().to_string(),
        meta["run_id"].as_str().unwrap()
    );
    assert_eq!((header.geometry.rows(), header.geometry.cols()), (24, 80));
    assert!(!header.input_recorded, "input is not recorded by default");
    let expected_term = std::env::var("TERM")
        .ok()
        .and_then(|t| tendr::recording::TermName::new(t).ok())
        .map(|t| t.as_str().to_owned())
        .unwrap_or_default();
    assert_eq!(header.term.as_str(), expected_term);

    let last = recording.records.last().unwrap().sequence.get();
    let state = &meta["pty"]["recording"];
    assert_eq!(state["state"], "Complete", "{meta}");
    assert_eq!(state["input_recorded"], false);
    assert_eq!(state["last_recorded_sequence"], last);
    assert_eq!(state["last_synced_sequence"], last);

    let dir = recording_dir(&root, "pty-rec");
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(dir.parent().unwrap()), 0o700);
    assert_eq!(mode(&dir), 0o700);
}

#[test]
fn an_applied_resize_is_recorded_between_the_output_around_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let sock = start_cat(&root, "pty-rec-resize");
    let _kill = harness::SessionGuard::new(&root, "pty-rec-resize");

    let mut human = attach_as_human(&sock);
    write_msg(&mut human, MSG_DATA, b"one\n");
    wait_log_contains(&root, "pty-rec-resize", "one");
    write_msg(
        &mut human,
        MSG_RESIZE,
        &resize_payload(Geometry::new(30, 100).unwrap()),
    );
    write_msg(&mut human, MSG_DATA, b"two\n");
    wait_log_contains(&root, "pty-rec-resize", "two");
    write_msg(&mut human, MSG_DETACH, &[]);
    drop(human);
    tendr(&root)
        .args(["kill", "pty-rec-resize", "--force"])
        .output()
        .unwrap();
    harness::wait_terminal(&root, "pty-rec-resize");

    let recording = decode_session_recording(&root, "pty-rec-resize");
    let resizes: Vec<usize> = recording
        .records
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r.kind, tendr::recording::RecordKind::Resize { .. }))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(resizes.len(), 1, "{:?}", recording.records);
    let at = resizes[0];
    assert_eq!(
        recording.records[at].kind,
        tendr::recording::RecordKind::Resize {
            geometry: tendr::recording::Geometry::new(30, 100).unwrap(),
            cause: tendr::recording::ResizeCause::User,
        }
    );
    let before = recorded_output(&recording.records[..at]);
    let after = recorded_output(&recording.records[at + 1..]);
    assert!(contains(&before, b"one"), "before: {before:?}");
    assert!(!contains(&before, b"two"), "before: {before:?}");
    assert!(contains(&after, b"two"), "after: {after:?}");
}

#[test]
fn a_resize_with_a_zero_dimension_is_ignored() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "pty-zero-size", "--pty", "--stdin", "--", "sh"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-zero-size");
    let _kill = harness::SessionGuard::new(&root, "pty-zero-size");
    let sock = wait_for_attach_socket(&root, "pty-zero-size");

    let mut human = attach_as_human(&sock);
    // Built by hand: `resize_payload` cannot encode a zero dimension.
    write_msg(&mut human, MSG_RESIZE, &[0, 0, 0, 100]);
    write_msg(&mut human, MSG_DATA, b"stty size\n");
    wait_log_contains(&root, "pty-zero-size", "24 80");
    write_msg(&mut human, MSG_DETACH, &[]);
    drop(human);
    tendr(&root)
        .args(["kill", "pty-zero-size", "--force"])
        .output()
        .unwrap();
    harness::wait_terminal(&root, "pty-zero-size");

    let recording = decode_session_recording(&root, "pty-zero-size");
    assert!(
        !recording
            .records
            .iter()
            .any(|r| matches!(r.kind, tendr::recording::RecordKind::Resize { .. })),
        "{:?}",
        recording.records
    );
}

#[test]
fn a_recording_size_limit_stops_recording_but_not_the_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .env("TENDR_TEST_RECORDING_MAX_BYTES", "2048")
        .args(["start", "pty-rec-limit", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-rec-limit");
    let _kill = harness::SessionGuard::new(&root, "pty-rec-limit");

    // A small record first: how the PTY coalesces the flood into reads (and so
    // whether any of it fits under the limit) differs between platforms.
    push(&root, "pty-rec-limit", b"first\n");
    wait_log_contains(&root, "pty-rec-limit", "first");
    let line = format!("{}\n", "x".repeat(59));
    push(&root, "pty-rec-limit", line.repeat(50).as_bytes());

    let deadline = Instant::now() + Duration::from_secs(10);
    let meta = loop {
        let meta = session_meta(&root, "pty-rec-limit");
        if meta["pty"]["recording"]["state"] == "Stopped" {
            break meta;
        }
        assert!(Instant::now() < deadline, "recording never stopped: {meta}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(meta["status"], "Running", "the session keeps running");
    assert_eq!(meta["pty"]["recording"]["reason"], "size_limit");
    let last = meta["pty"]["recording"]["last_recorded_sequence"]
        .as_u64()
        .unwrap_or_else(|| panic!("the stop names the recorded prefix: {meta}"));

    // The event precedes the metadata flip.
    let events = harness::read_events(&root, "pty-rec-limit");
    let event = events
        .iter()
        .find(|e| e["kind"] == "recording.stopped")
        .expect("recording.stopped is logged before status shows it");
    assert_eq!(
        event["data"],
        serde_json::json!({"last_recorded_sequence": last, "reason": "size_limit"})
    );

    // Capture continues past the unrecorded suffix.
    push(&root, "pty-rec-limit", b"after-stop\n");
    wait_log_contains(&root, "pty-rec-limit", "after-stop");

    tendr(&root)
        .args(["kill", "pty-rec-limit", "--force"])
        .output()
        .unwrap();
    let meta = harness::wait_terminal(&root, "pty-rec-limit");
    assert_eq!(meta["pty"]["recording"]["state"], "Stopped", "{meta}");
    assert_eq!(meta["pty"]["recording"]["last_recorded_sequence"], last);
    assert!(
        meta["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("recording stopped")),
        "{meta}"
    );

    let recording = decode_session_recording(&root, "pty-rec-limit");
    assert_eq!(recording.records.last().unwrap().sequence.get(), last);
    assert!(!contains(
        &recorded_output(&recording.records),
        b"after-stop"
    ));
    let dir = recording_dir(&root, "pty-rec-limit");
    let total: u64 = tendr::recorder::DirectoryStore::segments(&dir)
        .unwrap()
        .iter()
        .map(|f| std::fs::metadata(f).unwrap().len())
        .sum();
    assert!(total <= 2048, "{total} bytes recorded");
}

/// A run's end invalidates its attach handles (PR #68 review, finding 1). The
/// attached client gets end of stream while the run's on-exit hook is still
/// running, and it can't touch the replacement run: before the fix, its late
/// detach rewrote the new run's `meta.json` back to `AgentControl` while a human
/// held the new run.
#[test]
fn a_finished_runs_attach_client_cannot_touch_its_replacement() {
    use std::io::Read;

    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let started = root.path().join("hook-started");
    let gate = root.path().join("hook-gate");
    // Holds run A's sidecar in its on-exit hook until the gate appears, or 20 s.
    let hook = format!(
        "sh -c {} {} {}",
        shell_words::quote(
            r#"touch "$0"; i=0; while [ ! -e "$1" ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done"#
        ),
        shell_words::quote(started.to_str().unwrap()),
        shell_words::quote(gate.to_str().unwrap()),
    );

    tendr(&root)
        .args([
            "start",
            "pty-end",
            "--pty",
            "--stdin",
            "--on-exit",
            &hook,
            "--",
            "cat",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "pty-end");
    let sock_a = wait_for_attach_socket(&root, "pty-end");
    let mut stale = attach_as_human(&sock_a);
    wait_for_pty_control(&root, "pty-end", "HumanControl");

    tendr(&root)
        .args(["kill", "--force", "pty-end"])
        .assert()
        .success();
    harness::poll_until(Duration::from_secs(10), Duration::from_millis(20), || {
        if started.exists() {
            harness::Observation::Ready(())
        } else {
            harness::Observation::Pending("run A's on-exit hook has not started".into())
        }
    })
    .unwrap();

    // 1. The run is over: its client sees end of stream now, not when the
    //    sidecar process finally exits after the hook.
    // EINVAL on macOS means the socket is already shut down; the read below
    // then returns end of stream at once (as in `read_until_closed`).
    let _ = stale.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 4096];
    loop {
        match stale.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(e) => panic!("run A's client should see end of stream, got {e:?}"),
        }
    }

    // 2. Replace the run and give the new one a human controller.
    tendr(&root)
        .args([
            "start",
            "pty-end",
            "--pty",
            "--stdin",
            "--replace",
            "--",
            "cat",
        ])
        .assert()
        .success();
    let sock_b = wait_for_attach_socket(&root, "pty-end");
    assert_ne!(sock_a, sock_b, "the replacement binds its own socket");
    let _human_b = attach_as_human(&sock_b);
    wait_for_pty_control(&root, "pty-end", "HumanControl");

    // 3. Run A's stale client goes away while A's sidecar is still alive.
    drop(stale);
    std::thread::sleep(Duration::from_millis(1500));
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            root.path()
                .join(".tendr/sessions/default/pty-end/meta.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        meta["pty"]["control"], "HumanControl",
        "run A must not rewrite run B's control owner: {meta}"
    );

    std::fs::write(&gate, b"").unwrap();
}

/// Input arriving on the stdin FIFO (how `exec` reaches a PTY python-repl)
/// while an agent `push` holds the PTY input waits its turn instead of being
/// dropped (PR #68 review, finding 2).
#[test]
fn fifo_input_waits_behind_an_agent_push() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-fifo");
    let go = root.path().join("go");

    // Consumes 1 KiB, reports READY, then stops reading until the go file
    // exists, so the push below stays in flight holding the input.
    let script = "stty raw -echo; head -c 1024 >/dev/null; printf READY; \
                  while [ ! -f \"$GO\" ]; do sleep 0.05; done; exec cat";
    tendr(&root)
        .args(["start", "pty-fifo", "--pty", "--stdin"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-fifo");

    let mut payload = vec![b'a'; 1 << 20];
    payload.extend_from_slice(b"\nPUSH-TAIL\n");
    let mut agent = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["push", "pty-fifo"])
        .env("HOME", root.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut agent_stdin = agent.stdin.take().unwrap();
    let feeder = std::thread::spawn(move || {
        let _ = agent_stdin.write_all(&payload);
    });
    wait_log_contains(&root, "pty-fifo", "READY");
    assert!(
        agent.try_wait().unwrap().is_none(),
        "setup invariant: the push must still hold the input"
    );

    // An exec-style frame through the FIFO while the push holds the input.
    let fifo = root
        .path()
        .join(".tendr/sessions/default/pty-fifo/stdin.pipe");
    let writer = std::thread::spawn(move || {
        let mut f = std::fs::OpenOptions::new().write(true).open(fifo).unwrap();
        f.write_all(b"FIFO-LINE\n").unwrap();
    });

    std::fs::write(&go, b"").unwrap();
    feeder.join().unwrap();
    let status = agent.wait().unwrap();
    assert!(status.success(), "the push itself completes");
    writer.join().unwrap();

    let log = wait_log_contains(&root, "pty-fifo", "FIFO-LINE");
    assert!(log.contains("PUSH-TAIL"), "the push was written in full");
    assert!(
        log.find("PUSH-TAIL") < log.find("FIFO-LINE"),
        "the FIFO input waits for the push rather than interleaving"
    );
}

/// A FIFO writer (an `exec` frame) closes as soon as its bytes are in the pipe,
/// so its hang-up is how a complete frame ends, not a sign it was abandoned.
/// Bytes still waiting on a full PTY after the writer has gone must be
/// delivered once the child reads again, and the agent claim then released
/// (PR #68 review, finding 6: not changed, because cancelling on hang-up would
/// drop them).
///
/// How much a PTY and a pipe buffer differs by platform (a Linux PTY absorbs
/// tens of KiB), so the test assumes no size. It writes until the FIFO itself
/// stays full, which can only happen once the forwarder is stuck on a full
/// PTY, and closes the writer there.
#[test]
fn fifo_input_is_delivered_after_its_writer_closes() {
    use std::os::unix::fs::OpenOptionsExt;

    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-fifo-gone");
    let go = root.path().join("go");
    let out = root.path().join("received");

    // Reads nothing until the go file exists, then copies its input to a file.
    let script = "stty raw -echo; printf READY; \
                  while [ ! -f \"$GO\" ]; do sleep 0.05; done; exec cat > \"$OUT\"";
    tendr(&root)
        .args(["start", "pty-fifo-gone", "--pty", "--stdin"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .arg("--env")
        .arg(format!("OUT={}", out.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-fifo-gone");
    wait_log_contains(&root, "pty-fifo-gone", "READY");

    // Nonblocking, so a full FIFO is observed instead of waited on. The open
    // is refused (ENXIO) until the forwarder has the FIFO open for reading.
    let fifo_path = root
        .path()
        .join(".tendr/sessions/default/pty-fifo-gone/stdin.pipe");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut fifo = loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo_path)
        {
            Ok(f) => break f,
            Err(e) => assert!(Instant::now() < deadline, "open stdin.pipe: {e}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Fill the FIFO until it has stayed full for a whole second. The forwarder
    // drains it as fast as the PTY takes input, so a FIFO that stays full means
    // the PTY is full and the forwarder is stuck mid-frame.
    let chunk = [b'a'; 1024];
    let mut written = 0usize;
    let mut last_progress = Instant::now();
    while last_progress.elapsed() < Duration::from_secs(1) {
        assert!(Instant::now() < deadline, "the FIFO never stayed full");
        match fifo.write(&chunk) {
            Ok(n) => {
                written += n;
                last_progress = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("write stdin.pipe: {e}"),
        }
    }
    drop(fifo); // the writer is gone; its frame is not all delivered

    let early = tendr(&root)
        .args(["push", "pty-fifo-gone"])
        .write_stdin("EARLY\n")
        .output()
        .unwrap();
    let early_err = String::from_utf8_lossy(&early.stderr);
    assert!(
        !early.status.success() && early_err.contains("(Agent)"),
        "the waiting frame still holds the input after its writer closed: {early_err}"
    );

    // Once the child reads again, every byte of the frame arrives, and then
    // the frame no longer holds the input.
    std::fs::write(&go, b"").unwrap();
    let received = || std::fs::metadata(&out).map_or(0, |m| m.len() as usize);
    while received() < written {
        assert!(
            Instant::now() < deadline,
            "{} of {written} bytes delivered",
            received()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let after = tendr(&root)
        .args(["push", "pty-fifo-gone"])
        .write_stdin("AFTER\n")
        .output()
        .unwrap();
    assert!(
        after.status.success(),
        "push after the frame: {}",
        String::from_utf8_lossy(&after.stderr)
    );
    while received() < written + 6 {
        assert!(Instant::now() < deadline, "the later push never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(bytes.len(), written + 6);
    assert!(bytes[..written].iter().all(|&b| b == b'a'));
    assert_eq!(&bytes[written..], b"AFTER\n");
}
