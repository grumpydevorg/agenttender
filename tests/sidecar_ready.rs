mod harness;

use harness::{DeadlineAssertExt, tender, wait_terminal};
use predicates::prelude::*;
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn start_returns_promptly_not_blocked_by_child() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Returning at all — under the shared hang deadline, well before the 60s
    // child exits — proves `start` did not block on the child, i.e. the ready
    // pipe fd was not leaked into it. No fragile wall-clock upper bound needed.
    tender(&root)
        .args(["start", "prompt-test", "sleep", "60"])
        .assert_within_deadline()
        .success();

    tender(&root)
        .args(["kill", "--force", "prompt-test"])
        .assert()
        .success();
}

#[test]
fn start_creates_session_and_returns_json() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let output = tender(&root)
        .args(["start", "test-job", "echo", "hello"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let meta: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(meta["session"], "test-job");
    assert_eq!(meta["schema_version"], 1);
    assert_eq!(meta["status"], "Running");
    assert!(meta["run_id"].is_string());
    assert!(meta["sidecar"]["pid"].is_number());
    assert!(meta["child"]["pid"].is_number());
    assert_eq!(meta["launch_spec"]["argv"][0], "echo");
    assert_eq!(meta["launch_spec"]["argv"][1], "hello");
}

#[test]
fn start_writes_durable_meta_json() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "durable-test", "echo", "hi"])
        .assert()
        .success();

    let meta_path = root
        .path()
        .join(".tender/sessions/default/durable-test/meta.json");
    assert!(meta_path.exists(), "meta.json not written to disk");

    let content = std::fs::read_to_string(&meta_path).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(meta["session"], "durable-test");
}

#[test]
fn start_same_name_after_completed_fails_already_exists() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "dup-test", "echo", "a"])
        .assert()
        .success();

    tender(&root)
        .args(["start", "dup-test", "echo", "b"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("terminal state")
                .or(predicate::str::contains("session conflict")),
        );
}

#[test]
fn status_reads_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "status-test", "echo", "hi"])
        .assert()
        .success();

    tender(&root)
        .args(["status", "status-test"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""session": "status-test"#));
}

#[test]
fn status_nonexistent_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root).args(["status", "nope"]).assert().failure();
}

#[test]
fn list_shows_sessions() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Empty list
    let output = tender(&root).args(["list"]).output().unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert!(entries.is_empty());

    // Create sessions
    tender(&root)
        .args(["start", "bravo", "echo", "b"])
        .assert()
        .success();
    tender(&root)
        .args(["start", "alpha", "echo", "a"])
        .assert()
        .success();

    let output = tender(&root).args(["list"]).output().unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "bravo"]);
}

#[test]
fn launch_spec_json_cleaned_up() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "cleanup-test", "echo", "hi"])
        .assert()
        .success();

    let spec_path = root
        .path()
        .join(".tender/sessions/default/cleanup-test/launch_spec.json");
    // Wait for sidecar to clean up
    wait_terminal(&root, "cleanup-test");
    assert!(!spec_path.exists(), "launch_spec.json should be cleaned up");
}

#[test]
fn lock_released_after_sidecar_exits() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "lock-test", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "lock-test");

    #[cfg(unix)]
    {
        let lock_path = root.path().join(".tender/sessions/default/lock-test/lock");
        if lock_path.exists() {
            use std::fs::File;
            use std::os::unix::io::AsRawFd;
            let file = File::open(&lock_path).unwrap();
            let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            assert_eq!(ret, 0, "lock should be released after sidecar exits");
        }
    }
}
