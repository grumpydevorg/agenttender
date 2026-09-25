//! Readiness is a courtesy to the `start` client, never a condition of the run
//! (issue #71). A client that dies between the sidecar's spawn and its
//! readiness write must not end supervision: the session still reaches its
//! terminal state, logs `run.exited`, runs its `--on-exit` hooks, and records
//! that readiness went undelivered.
//!
//! The window is opened deterministically, not by timing: the debug-only
//! `TENDER_TEST_READY_GATE` hook holds the sidecar just before its readiness
//! write until the gate file exists. The test waits for the session's meta on
//! disk (proof the sidecar is running), confirms the client is still blocked on
//! readiness, kills and reaps it, and only then opens the gate.

mod harness;

use harness::{read_events, tender, touch_cmd};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tender::model::ids::{Namespace, SessionName};
use tender::session::{self, LockGuard, SessionRoot};

static SERIAL: Mutex<()> = Mutex::new(());

const READINESS_WARNING: &str = "readiness not delivered: start client gone";

fn meta_path(root: &TempDir, session: &str) -> PathBuf {
    root.path()
        .join(format!(".tender/sessions/default/{session}/meta.json"))
}

/// Spawn `tender start <args>` with the readiness gate closed.
fn spawn_gated_start(root: &TempDir, gate: &std::path::Path, args: &[&str]) -> Child {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("tender"));
    cmd.arg("start")
        .args(args)
        .env("HOME", root.path())
        .env("TENDER_TEST_READY_GATE", gate)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Mirror harness::tender: Git-for-Windows coreutils for `sleep`/`true`.
    #[cfg(windows)]
    {
        let git_usr_bin = std::path::Path::new(r"C:\Program Files\Git\usr\bin");
        if git_usr_bin.exists() {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{};{path}", git_usr_bin.display()));
        }
    }
    cmd.spawn().expect("spawn tender start")
}

/// Start `session`, then lose its client inside the spawn-to-readiness window:
/// once the sidecar has written meta (it holds the lock and the ready pipe), the
/// client — still blocked on readiness — is killed and reaped before the gate
/// opens, so the sidecar's readiness write has no reader.
fn start_and_lose_client(root: &TempDir, session: &str, rest: &[&str]) {
    let gate = root.path().join(format!("ready-gate-{session}"));
    let mut args = vec![session];
    args.extend_from_slice(rest);
    let mut client = spawn_gated_start(root, &gate, &args);

    let deadline = Instant::now() + Duration::from_secs(10);
    while !meta_path(root, session).exists() {
        if let Some(status) = client.try_wait().expect("poll start client") {
            panic!("start client exited ({status}) before the sidecar wrote meta");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {session}'s sidecar to write meta"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        client.try_wait().expect("poll start client").is_none(),
        "start client must still be blocked on readiness when it is killed"
    );
    client.kill().expect("kill start client");
    let status = client.wait().expect("reap start client");
    assert!(!status.success(), "start client was killed, not completed");

    std::fs::write(&gate, b"").expect("open readiness gate");
}

/// Wait until the sidecar has let go of the session (its lock is ours), then
/// return the meta it left behind. Holding the guard keeps the observation
/// stable while the caller asserts. A sidecar that dies at the readiness write
/// releases the lock at once, leaving non-terminal meta: exactly the defect.
fn wait_sidecar_gone(root: &TempDir, session: &str) -> (serde_json::Value, LockGuard) {
    let session_root = SessionRoot::new(root.path().join(".tender/sessions"));
    let namespace = Namespace::new("default").unwrap();
    let name = SessionName::new(session).unwrap();
    let dir = session::open(&session_root, &namespace, &name)
        .expect("open session")
        .expect("session exists");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match LockGuard::try_acquire(&dir) {
            Ok(lock) => {
                let content = std::fs::read_to_string(meta_path(root, session)).unwrap();
                return (serde_json::from_str(&content).unwrap(), lock);
            }
            Err(session::SessionError::Locked(_)) => {}
            Err(error) => panic!("failed to acquire session lock: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {session}'s sidecar to release its lock"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn warnings(meta: &serde_json::Value) -> Vec<String> {
    meta["warnings"]
        .as_array()
        .map(|w| {
            w.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn assert_readiness_warning(meta: &serde_json::Value) {
    let warnings = warnings(meta);
    assert!(
        warnings.iter().any(|w| w.starts_with(READINESS_WARNING)),
        "expected a durable '{READINESS_WARNING}' warning, got {warnings:?}"
    );
}

fn assert_exited_ok_with_event(root: &TempDir, session: &str, meta: &serde_json::Value) {
    assert_eq!(
        meta["status"], "Exited",
        "the sidecar let go of {session} before the run ended (SidecarLost residue): {meta}"
    );
    assert_eq!(meta["reason"], "ExitedOk");
    let kinds: Vec<String> = read_events(root, session)
        .iter()
        .filter_map(|e| e["kind"].as_str().map(str::to_owned))
        .collect();
    assert!(
        kinds.iter().any(|k| k == "run.exited"),
        "run.exited must be logged, got {kinds:?}"
    );
}

#[test]
fn client_lost_after_spawn_still_supervises_to_exit() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    start_and_lose_client(&root, "s1", &["--", "sleep", "1"]);

    let (meta, _lock) = wait_sidecar_gone(&root, "s1");
    assert_exited_ok_with_event(&root, "s1", &meta);
    assert_readiness_warning(&meta);
}

#[test]
fn client_lost_after_spawn_still_runs_on_exit_hook() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let marker = root.path().join("on-exit-marker");

    let hook = touch_cmd(&marker);
    start_and_lose_client(&root, "s1", &["--on-exit", &hook, "--", "sleep", "1"]);

    let (meta, lock) = wait_sidecar_gone(&root, "s1");
    assert_exited_ok_with_event(&root, "s1", &meta);
    drop(lock);

    // Hooks run after the sidecar releases the lock.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "the --on-exit hook never ran: {marker:?} missing"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn client_lost_during_dependency_wait_still_runs_after_deps() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["start", "dep1", "--", "sleep", "1"])
        .assert()
        .success();

    // The --after path signals readiness after its first dependency scan.
    start_and_lose_client(&root, "job2", &["--after", "dep1", "--", "true"]);

    let (meta, _lock) = wait_sidecar_gone(&root, "job2");
    assert_exited_ok_with_event(&root, "job2", &meta);
    assert_readiness_warning(&meta);
}

#[test]
fn client_lost_on_spawn_failure_records_the_warning() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    start_and_lose_client(&root, "bad", &["--", "nonexistent-command-xyz-12345"]);

    let (meta, _lock) = wait_sidecar_gone(&root, "bad");
    assert_eq!(meta["status"], "SpawnFailed");
    assert_readiness_warning(&meta);
}
