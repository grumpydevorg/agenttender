#![cfg(unix)]

mod harness;

use harness::tender;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tender::attach_proto::{
    MODE_ATTACH, MODE_PUSH, MODE_TAKEOVER, MSG_ACCEPTED, MSG_DATA, MSG_DETACH, MSG_HELLO,
    MSG_INPUT_DONE, MSG_REJECTED, MSG_RESIZE, MSG_RETIRED, PROTOCOL_VERSION, read_msg,
    resize_payload,
};

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

    let output = tender(&root)
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

    tender(&root)
        .args(["start", "pty-echo", "--pty", "--", "echo", "pty-hello"])
        .output()
        .unwrap();

    harness::wait_terminal(&root, "pty-echo");

    let output = tender(&root)
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

    tender(&root)
        .args(["start", "pty-meta", "--pty", "--", "echo", "hi"])
        .output()
        .unwrap();

    harness::wait_terminal(&root, "pty-meta");

    let output = tender(&root).args(["status", "pty-meta"]).output().unwrap();

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

    tender(&root)
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
    harness::wait_running(&root, "pty-shell");

    let output = tender(&root)
        .args(["exec", "pty-shell", "--", "echo", "test"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not supported") || stderr.contains("PTY"),
        "should reject exec on PTY: {stderr}"
    );

    tender(&root).args(["kill", "pty-shell"]).output().ok();
}

#[test]
fn attach_to_non_pty_session_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "pipe-session", "--", "sleep", "60"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pipe-session");

    let output = tender(&root)
        .args(["attach", "pipe-session"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("PTY") || stderr.contains("not PTY"),
        "should reject attach on non-PTY: {stderr}"
    );

    tender(&root).args(["kill", "pipe-session"]).output().ok();
}

#[test]
fn attach_socket_exists_for_pty_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "pty-attach", "--pty", "--", "sleep", "60"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-attach");

    let breadcrumb = root
        .path()
        .join(".tender/sessions/default/pty-attach/a.sock.path");

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

    tender(&root).args(["kill", "pty-attach"]).output().ok();
}

#[test]
fn push_to_pty_session_delivers_input() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start a PTY cat session with stdin
    tender(&root)
        .args(["start", "pty-push", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-push");

    // Push some input
    tender(&root)
        .args(["push", "pty-push"])
        .write_stdin(b"hello-from-push\n")
        .output()
        .unwrap();

    // Poll the log until the pushed input echoes through the PTY (no fixed sleep).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let output = tender(&root)
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

    tender(&root).args(["kill", "pty-push"]).output().ok();
}

/// Python REPL exec works on PTY sessions.
#[test]
fn exec_python_pty() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
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
    harness::wait_running(&root, "py-pty");
    // No sleep: exec buffers the frame and waits for the result file, so the
    // REPL not being input-ready yet is a delay, not a lost command (PR #55).

    let output = tender(&root)
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

    let _ = tender(&root).args(["kill", "py-pty", "--force"]).assert();
}

/// PTY exec is still rejected for shell targets.
#[test]
fn exec_pty_still_rejected_for_shells() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
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
    harness::wait_running(&root, "pty-shell");

    tender(&root)
        .args(["exec", "pty-shell", "--", "echo", "test"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not supported on PTY"));

    let _ = tender(&root)
        .args(["kill", "pty-shell", "--force"])
        .assert();
}

/// Wait for the attach socket breadcrumb and return the socket path.
fn wait_for_attach_socket(root: &TempDir, session: &str) -> std::path::PathBuf {
    let breadcrumb = root
        .path()
        .join(format!(".tender/sessions/default/{session}/a.sock.path"));
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
fn hello(sock_path: &std::path::Path, mode: u8) -> (UnixStream, (u8, Vec<u8>)) {
    let mut stream = UnixStream::connect(sock_path).expect("failed to connect to attach socket");
    write_msg(&mut stream, MSG_HELLO, &[PROTOCOL_VERSION, mode]);
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
    let (stream, (msg_type, payload)) = hello(sock_path, MODE_ATTACH);
    assert_eq!(
        msg_type,
        MSG_ACCEPTED,
        "attach should be accepted: {}",
        String::from_utf8_lossy(&payload)
    );
    stream
}

/// Poll `tender log --raw` until it contains `needle`.
fn wait_log_contains(root: &TempDir, session: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = tender(root)
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
        .join(format!(".tender/sessions/default/{session}/meta.json"));
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
    tender(&root)
        .args(["start", "pty-hc", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-hc");

    let sock_path = wait_for_attach_socket(&root, "pty-hc");

    // Simulate a human attaching
    let _human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-hc", "HumanControl");

    // Push should be rejected
    let output = tender(&root)
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
    let output = tender(&root)
        .args(["push", "pty-hc"])
        .write_stdin(b"accepted\n")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "push should succeed after detach: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    tender(&root).args(["kill", "pty-hc"]).output().ok();
}

#[test]
fn attach_contention_rejected() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "pty-contend", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-contend");

    let sock_path = wait_for_attach_socket(&root, "pty-contend");

    // First human attaches
    let _human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-contend", "HumanControl");

    // Second attach via CLI should be rejected
    let output = tender(&root)
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
    tender(&root).args(["kill", "pty-contend"]).output().ok();
}

#[test]
fn resize_reaches_child_pty() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // An interactive shell so we can query the child's terminal size post-resize.
    tender(&root)
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
    write_msg(&mut stream, MSG_RESIZE, &resize_payload(40, 120));
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
    let status = tender(&root)
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
    tender(&root)
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

    tender(&root)
        .args(["start", "pty-ev", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
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
        assert_eq!(event["source"], "tender.sidecar");
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

    tender(&root).args(["kill", "pty-ev"]).output().ok();
}

// --- Cloud PTY control, slice 1: sidecar-enforced input authority ---

/// Force-kills a session when dropped, so a failing assertion cannot leak a
/// running sidecar and child.
struct KillOnDrop<'a> {
    root: &'a TempDir,
    session: &'static str,
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = tender(self.root)
            .args(["kill", self.session, "--force"])
            .output();
    }
}

/// Run the real `tender attach` CLI inside a PTY, as a terminal user would.
struct CliAttach {
    child: <tender::platform::Current as tender::platform::Platform>::SupervisedChild,
    input: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl CliAttach {
    fn spawn(root: &TempDir, args: &[&str]) -> Self {
        use tender::platform::{Current, Platform};
        let mut argv = vec![
            assert_cmd::cargo::cargo_bin("tender")
                .to_string_lossy()
                .into_owned(),
            "attach".to_owned(),
        ];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "HOME".to_owned(),
            root.path().to_string_lossy().into_owned(),
        );
        let mut child = Current::spawn_child_pty(&argv, None, &env).unwrap();
        let input = Current::child_stdin(&mut child).unwrap();
        let mut reader = Current::child_stdout(&mut child).unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = std::io::Read::read(&mut reader, &mut chunk) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        });
        Self {
            child,
            input,
            output,
        }
    }

    /// Type `line` until the session log shows it. Keystrokes typed before the CLI
    /// enters raw mode are discarded (`TCSAFLUSH`), so a single write races the
    /// handshake; the marker is idempotent, so retyping is safe.
    fn type_until_logged(&mut self, root: &TempDir, session: &str, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.input.write_all(line.as_bytes()).unwrap();
            let output = tender(root)
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
        use tender::platform::{Current, Platform};
        let kill = Current::child_kill_handle(&self.child);
        let _ = Current::kill_child(&kill, true);
        let _ = Current::child_wait(&mut self.child);
    }
}

#[test]
fn cli_attach_delivers_typed_input_through_the_handshake() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-cli",
    };
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
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-cli-take",
    };
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
    tender(root)
        .args(["start", session, "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    harness::wait_running(root, session);
    wait_for_attach_socket(root, session)
}

fn push(root: &TempDir, session: &str, bytes: &[u8]) {
    let output = tender(root)
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
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-nohello",
    };
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

    let status = tender(&root)
        .args(["status", "pty-nohello"])
        .output()
        .unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(meta["pty"]["control"], "AgentControl");

    tender(&root).args(["kill", "pty-nohello"]).output().ok();
}

#[test]
fn attach_is_accepted_with_an_epoch_and_a_second_attach_is_rejected() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-epoch",
    };
    let sock_path = start_cat(&root, "pty-epoch");

    let (mut first, (msg_type, payload)) = hello(&sock_path, MODE_ATTACH);
    assert_eq!(msg_type, MSG_ACCEPTED);
    assert_eq!(epoch_of(&payload), 1);
    wait_for_pty_control(&root, "pty-epoch", "HumanControl");

    let (mut second, (msg_type, payload)) = hello(&sock_path, MODE_ATTACH);
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
    tender(&root).args(["kill", "pty-epoch"]).output().ok();
}

#[test]
fn takeover_retires_the_previous_human_and_rejects_its_later_input() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-takeover",
    };
    let sock_path = start_cat(&root, "pty-takeover");

    let mut old = attach_as_human(&sock_path);
    let (mut new, (msg_type, payload)) = hello(&sock_path, MODE_TAKEOVER);
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
    tender(&root).args(["kill", "pty-takeover"]).output().ok();
}

#[test]
fn takeover_revokes_queued_agent_input() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-revoke",
    };
    let go = root.path().join("go");

    // The child consumes exactly 1 KiB of agent input, reports READY, then stops
    // reading until the go file exists. Everything the agent pushes after that
    // stays queued behind a full PTY input buffer.
    let script = "stty raw -echo; head -c 1024 >/dev/null; printf READY; \
                  while [ ! -f \"$GO\" ]; do sleep 0.05; done; exec cat";
    tender(&root)
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
    let mut agent = std::process::Command::new(assert_cmd::cargo::cargo_bin("tender"))
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

    let (mut human, (msg_type, _)) = hello(&sock_path, MODE_TAKEOVER);
    assert_eq!(msg_type, MSG_ACCEPTED);
    write_msg(&mut human, MSG_DATA, b"HUMAN-LINE\n");
    std::fs::write(&go, b"").unwrap();

    let log = wait_log_contains(&root, "pty-revoke", "HUMAN-LINE");
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
    tender(&root)
        .args(["kill", "pty-revoke", "--force"])
        .output()
        .ok();
}

#[test]
fn takeover_after_a_silent_connection_reaches_the_same_running_process() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-reconnect",
    };

    tender(&root)
        .args(["start", "pty-reconnect", "--pty", "--stdin", "--", "sh"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-reconnect");
    let sock_path = wait_for_attach_socket(&root, "pty-reconnect");
    let status = tender(&root)
        .args(["status", "pty-reconnect"])
        .output()
        .unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let child_pid = meta["child"]["pid"].as_u64().unwrap();

    // A controller that goes silent without closing, like a half-open SSH session.
    let _silent = attach_as_human(&sock_path);

    let (mut new, (msg_type, _)) = hello(&sock_path, MODE_TAKEOVER);
    assert_eq!(msg_type, MSG_ACCEPTED);
    write_msg(&mut new, MSG_DATA, b"echo \"pid=\"$$\n");

    let expected = format!("pid={child_pid}");
    wait_log_contains(&root, "pty-reconnect", &expected);

    drop(new);
    tender(&root)
        .args(["kill", "pty-reconnect", "--force"])
        .output()
        .ok();
}

#[test]
fn stalled_viewer_cannot_block_output_capture() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-stalled",
    };
    let go = root.path().join("go");

    // After the go file appears, the child writes ~10 MiB — more than one
    // viewer's queue budget — then a marker, then stays alive.
    let script = "while [ ! -f \"$GO\" ]; do sleep 0.05; done; \
                  yes 0123456789abcdef0123456789abcdef | head -c 10485760; \
                  echo; echo ALL-OUTPUT-DONE; sleep 60";
    tender(&root)
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
    // directly: `tender log` would re-parse ~10 MiB on every poll.
    let log_path = root
        .path()
        .join(".tender/sessions/default/pty-stalled/output.log");
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
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-twopush",
    };
    let go = root.path().join("go");
    // The child consumes exactly 1 KiB of the first push, reports READY, then
    // reads nothing until the go file exists: the first push stays in flight.
    let script = "stty raw -echo; head -c 1024 >/dev/null; printf READY; \
                  while [ ! -f \"$GO\" ]; do sleep 0.05; done; exec cat";
    tender(&root)
        .args(["start", "pty-twopush", "--pty", "--stdin"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-twopush");

    let mut first = std::process::Command::new(assert_cmd::cargo::cargo_bin("tender"))
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

    let second = tender(&root)
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
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-private",
    };
    let sock_path = start_cat(&root, "pty-private");

    let sockets = root.path().join(".tender").join("sockets");
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
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-bigframe",
    };
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
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-slowhello",
    };
    let sock_path = start_cat(&root, "pty-slowhello");

    // A well-formed hello sent one byte per second: every individual read is
    // quick, but the whole handshake takes 7 s, past the 5 s deadline.
    let mut slow = UnixStream::connect(&sock_path).unwrap();
    let hello = [MSG_HELLO, 0, 0, 0, 2, PROTOCOL_VERSION, MODE_ATTACH];
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
    let status = tender(&root)
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
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-unsafe",
    };
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::create_dir_all(root.path().join(".tender")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.path().join(".tender/sockets")).unwrap();

    let output = tender(&root)
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
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-full-detach",
    };
    // The child never reads its input, so the PTY input buffer fills.
    tender(&root)
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
    let (_next, (msg_type, reason)) = hello(&sock_path, MODE_ATTACH);
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
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-push-gone",
    };
    tender(&root)
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
    let (mut agent, (msg_type, _)) = hello(&sock_path, MODE_PUSH);
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
        let (_next, (msg_type, reason)) = hello(&sock_path, MODE_ATTACH);
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
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-push-slots",
    };
    let sock_path = start_cat(&root, "pty-push-slots");

    // More rounds than there are connection slots. Each push stays connected and
    // idle; the client never closes it.
    let mut idle_pushes = Vec::new();
    for round in 0..9 {
        let (mut push, (msg_type, reason)) = hello(&sock_path, MODE_PUSH);
        assert_eq!(
            msg_type,
            MSG_ACCEPTED,
            "push {round}: {}",
            String::from_utf8_lossy(&reason)
        );
        let (mut human, (msg_type, reason)) = hello(&sock_path, MODE_TAKEOVER);
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
        assert_eq!(
            tender::attach_proto::parse_input_done(&done.1).map(|(s, _, _)| s),
            Some(tender::attach_proto::INPUT_REVOKED)
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
    let _kill = KillOnDrop {
        root: &root,
        session: "pty-replaced",
    };
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
    let first = tender(&root)
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
    tender(&root)
        .args(["kill", "pty-replaced", "--force"])
        .output()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.exists() {
        assert!(Instant::now() < deadline, "exit callback never started");
        std::thread::sleep(Duration::from_millis(20));
    }

    // A replacement starts while the old sidecar is still in its callback.
    tender(&root)
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
        .join(format!(".tender/callbacks/{old_run_id}.json"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !record.exists() {
        assert!(Instant::now() < deadline, "old sidecar never finished");
        std::thread::sleep(Duration::from_millis(20));
    }

    let session = root.path().join(".tender/sessions/default/pty-replaced");
    assert!(!old_socket.exists(), "the old run's socket is gone");
    assert_eq!(
        tender::attach_proto::read_sock_path(&session),
        Some(new_socket),
        "the old run's cleanup removed the replacement's breadcrumb"
    );
}
