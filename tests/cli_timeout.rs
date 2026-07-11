mod harness;

use harness::{tender, wait_running, wait_terminal};
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn timeout_kills_child() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "--timeout", "2", "timeout-job", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "timeout-job");

    // `wait_terminal` has a 10-second deadline while the child sleeps for 60
    // seconds, so reaching terminal state proves that the timeout fired. Do
    // not include `tender start` latency in a sub-5-second assertion: detached
    // process startup takes about three seconds on hosted Windows runners and
    // is outside the configured child-runtime timeout.
    let meta = wait_terminal(&root, "timeout-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "TimedOut");
}

#[test]
fn timeout_not_triggered() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "--timeout", "60", "fast-job", "true"])
        .assert()
        .success();

    let meta = wait_terminal(&root, "fast-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "ExitedOk");
}
