#![cfg(unix)]

mod harness;

use harness::tender;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

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

    // Give cat time to echo through PTY
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Check log for the pushed content
    let output = tender(&root)
        .args(["log", "pty-push", "--raw"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello-from-push"),
        "push input should appear in PTY log: {stdout}"
    );

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
    std::thread::sleep(std::time::Duration::from_millis(1000));

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

/// Connect to the attach socket and hold the connection (simulating a human).
/// Returns the stream so the caller can control when it disconnects.
fn attach_as_human(sock_path: &std::path::Path) -> UnixStream {
    UnixStream::connect(sock_path).expect("failed to connect to attach socket")
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
fn resize_message_accepted() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "pty-resize", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-resize");

    let sock_path = wait_for_attach_socket(&root, "pty-resize");
    let mut stream = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-resize", "HumanControl");

    // Send a resize message (40 rows, 120 cols)
    let rows: u16 = 40;
    let cols: u16 = 120;
    let mut payload = [0u8; 4];
    payload[0..2].copy_from_slice(&rows.to_be_bytes());
    payload[2..4].copy_from_slice(&cols.to_be_bytes());

    // Wire format: 1 byte type + 4 byte length + payload
    let msg_type: u8 = 0x02; // MSG_RESIZE
    let len: u32 = 4;
    stream.write_all(&[msg_type]).unwrap();
    stream.write_all(&len.to_be_bytes()).unwrap();
    stream.write_all(&payload).unwrap();
    stream.flush().unwrap();

    // Give the sidecar time to process the resize
    std::thread::sleep(std::time::Duration::from_millis(200));

    // If the resize crashed the sidecar, the session would no longer be Running.
    // Verify the session is still alive and under human control.
    let output = tender(&root)
        .args(["status", "pty-resize"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let meta: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        meta["status"], "Running",
        "session should still be running after resize"
    );
    assert_eq!(meta["pty"]["control"], "HumanControl");

    // Verify the resize was actually applied by checking the PTY's window size.
    // We can read it via stty on the child side, but that requires a shell.
    // Instead, open a fresh PTY master fd check — we trust the ioctl works if
    // the session survives. The unit-level assertion is that the ioctl doesn't
    // crash and the session stays Running.

    drop(stream);
    tender(&root).args(["kill", "pty-resize"]).output().ok();
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
