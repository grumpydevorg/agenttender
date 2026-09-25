mod harness;

use harness::{tendr, wait_running, wait_terminal};
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn force_kill_produces_killed_forced() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "fk-job", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "fk-job");

    tendr(&root)
        .args(["kill", "--force", "fk-job"])
        .assert()
        .success();

    let meta = wait_terminal(&root, "fk-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "KilledForced");
}

#[test]
fn graceful_kill_produces_killed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "gk-job", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "gk-job");

    tendr(&root).args(["kill", "gk-job"]).assert().success();

    let meta = wait_terminal(&root, "gk-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "Killed");
}
