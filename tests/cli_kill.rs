mod harness;

use harness::{tendr, wait_running, wait_terminal};
use predicates::prelude::*;
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn kill_running_process() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "kill-job", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "kill-job");

    tendr(&root)
        .args(["kill", "kill-job"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status": "Exited"#))
        .stdout(predicate::str::contains(r#""reason": "Killed"#));
}

#[test]
fn kill_force() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "force-job", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "force-job");

    tendr(&root)
        .args(["kill", "--force", "force-job"])
        .assert()
        .success();

    let meta = wait_terminal(&root, "force-job");
    assert_eq!(meta["status"], "Exited");
}

#[test]
fn kill_already_dead_is_idempotent() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "dead-job", "true"])
        .assert()
        .success();
    wait_terminal(&root, "dead-job");

    tendr(&root)
        .args(["kill", "dead-job"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status": "Exited"#))
        .stdout(predicate::str::contains(r#""reason": "ExitedOk"#));
}

#[test]
fn kill_nonexistent_session_is_idempotent() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["kill", "nope"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not_found"));
}

#[test]
fn kill_preserves_child_identity() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let start_out = tendr(&root)
        .args(["start", "preserve-kill", "sleep", "60"])
        .output()
        .unwrap();
    let start_meta: serde_json::Value =
        serde_json::from_slice(&start_out.stdout).expect("not JSON");
    let start_pid = start_meta["child"]["pid"].as_u64().unwrap();
    wait_running(&root, "preserve-kill");

    let kill_out = tendr(&root)
        .args(["kill", "preserve-kill"])
        .output()
        .unwrap();
    let kill_meta: serde_json::Value = serde_json::from_slice(&kill_out.stdout).expect("not JSON");
    let kill_pid = kill_meta["child"]["pid"].as_u64().unwrap();

    assert_eq!(start_pid, kill_pid);
}

#[test]
fn status_shows_killed_after_kill() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "status-kill", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "status-kill");

    tendr(&root)
        .args(["kill", "status-kill"])
        .assert()
        .success();

    tendr(&root)
        .args(["status", "status-kill"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status": "Exited"#))
        .stdout(predicate::str::contains(r#""reason": "Killed"#));
}
