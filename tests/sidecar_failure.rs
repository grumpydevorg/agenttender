//! The lifecycle guard: a fault at any step after the child is spawned either
//! recovers with a visible warning, or ends the run with the child stopped and
//! `Exited { reason: SidecarFailed { step } }` recorded, loud everywhere:
//! meta, the `run.sidecar_failed` event, `wait`'s exit code 5 and the
//! `--on-exit` hooks (`TENDER_EXIT_REASON=SidecarFailed`).
//!
//! Faults are injected by the debug-only `TENDER_TEST_FAIL=<point>[,…]` and
//! `TENDER_TEST_PANIC=<point>` hooks. `TENDER_TEST_FAULT_GATE` holds a named
//! fault until a file exists, so on Unix a row can wait for the child's own
//! child (a grandchild in the same process group) before the fault fires, and
//! then prove the whole group was stopped. No timing races.

mod harness;

use harness::{echo_env_cmd, read_events, tender, touch_cmd};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tender::model::ids::{Namespace, ProcessIdentity, SessionName};
use tender::platform::{Current, Platform, ProcessStatus};
use tender::session::{self, LockGuard, SessionRoot};

static SERIAL: Mutex<()> = Mutex::new(());

fn session_dir(root: &TempDir, session: &str) -> PathBuf {
    root.path()
        .join(format!(".tender/sessions/default/{session}"))
}

/// How the fault is injected.
#[derive(Clone, Copy)]
enum Inject {
    Fail(&'static str),
    Panic(&'static str),
}

/// `tender start` in the background, so a row can act while the client is
/// still blocked on readiness.
fn spawn_start(root: &TempDir, inject: Inject, gate: &Path, args: &[&str]) -> Child {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("tender"));
    cmd.arg("start")
        .args(args)
        .env("HOME", root.path())
        .env("TENDER_TEST_FAULT_GATE", gate)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match inject {
        Inject::Fail(points) => cmd.env("TENDER_TEST_FAIL", points),
        Inject::Panic(point) => cmd.env("TENDER_TEST_PANIC", point),
    };
    // Mirror harness::tender: Git-for-Windows coreutils for `sh`/`sleep`.
    #[cfg(windows)]
    {
        let git_usr_bin = Path::new(r"C:\Program Files\Git\usr\bin");
        if git_usr_bin.exists() {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{};{path}", git_usr_bin.display()));
        }
    }
    cmd.spawn().expect("spawn tender start")
}

fn wait_for_file(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Reap the start client (bounded) and return (exit code, stdout, stderr).
fn finish_client(mut client: Child) -> (Option<i32>, String, String) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while client.try_wait().expect("poll start client").is_none() {
        if Instant::now() >= deadline {
            let _ = client.kill();
            panic!("start client did not return");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = client.wait_with_output().expect("collect start output");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Wait until the sidecar has let go of the session (its lock is ours) and
/// return the meta it left, holding the lock so the observation is stable.
fn wait_sidecar_gone(root: &TempDir, session: &str) -> (serde_json::Value, LockGuard) {
    let session_root = SessionRoot::new(root.path().join(".tender/sessions"));
    let namespace = Namespace::new("default").unwrap();
    let name = SessionName::new(session).unwrap();
    let dir = session::open(&session_root, &namespace, &name)
        .expect("open session")
        .expect("session exists");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match LockGuard::try_acquire(&dir) {
            Ok(lock) => {
                let path = session_dir(root, session).join("meta.json");
                let meta = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
                return (meta, lock);
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

fn assert_warning(meta: &serde_json::Value, needle: &str) {
    let warnings = warnings(meta);
    assert!(
        warnings.iter().any(|w| w.contains(needle)),
        "expected a warning containing {needle:?}, got {warnings:?}"
    );
}

fn child_identity(meta: &serde_json::Value) -> ProcessIdentity {
    serde_json::from_value(meta["child"].clone()).expect("meta carries the child identity")
}

/// The child is gone: not merely signalled, but no longer a live process.
fn assert_process_gone(id: &ProcessIdentity, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Current::process_status(id) {
            ProcessStatus::Missing | ProcessStatus::IdentityMismatch => return,
            status => assert!(
                Instant::now() < deadline,
                "{what} (pid {}) is still alive: {status:?}",
                id.pid
            ),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A grandchild the test only knows by pid: gone once `kill(pid, 0)` fails
/// (reparented, so its reaper collects it promptly).
#[cfg(unix)]
fn assert_pid_gone(pid_file: &Path, what: &str) {
    let pid: i32 = std::fs::read_to_string(pid_file)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    // SAFETY: signal 0 only probes for existence.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "{what} (pid {pid}) survived the group kill"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn event_kinds(root: &TempDir, session: &str) -> Vec<String> {
    read_events(root, session)
        .iter()
        .filter_map(|e| e["kind"].as_str().map(str::to_owned))
        .collect()
}

fn wait_file_content(path: &Path) -> String {
    wait_for_file(path, "the --on-exit hook");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        if !content.is_empty() || Instant::now() >= deadline {
            return content;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Everything a `SidecarFailed { step }` must show: meta, event, `wait`'s exit
/// code, the hook's `TENDER_EXIT_REASON`, a stopped child, no breadcrumb.
fn assert_sidecar_failed(root: &TempDir, session: &str, step: &str, error: &str, hook_out: &Path) {
    let (meta, lock) = wait_sidecar_gone(root, session);
    assert_eq!(meta["status"], "Exited", "{meta}");
    assert_eq!(meta["reason"], "SidecarFailed", "{meta}");
    assert_eq!(meta["step"], step, "{meta}");
    assert_warning(&meta, &format!("sidecar failed at {step}: "));
    assert_warning(&meta, error);
    assert_eq!(meta["transition_provenance"]["kind"], "direct");
    assert!(
        meta["transition_provenance"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "supervision_failed"),
        "{meta}"
    );
    assert_process_gone(&child_identity(&meta), "the child");
    assert!(
        !session_dir(root, session).join("child_pid").exists(),
        "the breadcrumb goes once the child is stopped and the record is durable"
    );

    let events = read_events(root, session);
    let terminal = events
        .iter()
        .rev()
        .find(|e| e["kind"].as_str().is_some_and(|k| k.starts_with("run.")))
        .expect("a lifecycle event");
    assert_eq!(terminal["kind"], "run.sidecar_failed", "{events:?}");
    assert_eq!(terminal["data"]["reason"], "SidecarFailed");
    assert_eq!(terminal["data"]["step"], step);
    drop(lock);

    // Hooks run after the lock is released.
    let hook = wait_file_content(hook_out);
    assert_eq!(hook.trim(), format!("{session} default SidecarFailed"));

    tender(root)
        .args(["wait", "--timeout", "5", session])
        .assert()
        .code(5);
}

/// A child that leaves a grandchild in its process group, and records both
/// pids. With `quiet`, it closes its output first, so capture ends at once
/// and supervision moves on to waiting for the exit.
#[cfg(unix)]
fn family_child(dir: &Path, quiet: bool) -> (Vec<String>, PathBuf) {
    let gpid = dir.join("grandchild.pid");
    let quiet = if quiet { "exec >/dev/null 2>&1; " } else { "" };
    let script = format!(
        "{quiet}sleep 60 & echo $! > {}; wait",
        shell_words::quote(gpid.to_str().unwrap())
    );
    (vec!["sh".into(), "-c".into(), script], gpid)
}

/// One row that ends the run: inject `inject` at `step`, let the child reach
/// a known state first, and check every channel. Returns start's exit code.
fn ends_the_run(
    inject: Inject,
    step: &str,
    error: &str,
    flags: &[&str],
    quiet_child: bool,
) -> Option<i32> {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let gate = root.path().join("fault-gate");
    let hook_out = root.path().join("hook-env");
    let hook = echo_env_cmd(&hook_out);

    #[cfg(unix)]
    let (argv, gpid) = family_child(root.path(), quiet_child);
    #[cfg(windows)]
    let argv: Vec<String> = {
        if quiet_child {
            vec![
                "sh".into(),
                "-c".into(),
                "exec >/dev/null 2>&1; sleep 60".into(),
            ]
        } else {
            vec!["sleep".into(), "60".into()]
        }
    };

    let mut args: Vec<&str> = vec!["s1", "--on-exit", &hook];
    args.extend_from_slice(flags);
    args.push("--");
    args.extend(argv.iter().map(String::as_str));
    let client = spawn_start(&root, inject, &gate, &args);

    #[cfg(unix)]
    wait_for_file(&gpid, "the grandchild to start");
    #[cfg(windows)]
    wait_for_file(
        &session_dir(&root, "s1").join("child_pid"),
        "the child to be spawned",
    );
    std::fs::write(&gate, b"").expect("open the fault gate");

    let (code, stdout, stderr) = finish_client(client);
    assert_sidecar_failed(&root, "s1", step, error, &hook_out);
    #[cfg(unix)]
    assert_pid_gone(&gpid, "the grandchild");

    if code == Some(5) {
        let snapshot: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("start printed no snapshot ({e}): {stdout} {stderr}"));
        assert_eq!(snapshot["reason"], "SidecarFailed", "{snapshot}");
    }
    code
}

// === Steps that end the run ===

#[test]
fn stdin_transport_failure_stops_the_child_and_records_sidecar_failed() {
    let code = ends_the_run(
        Inject::Fail("stdin_transport"),
        "stdin_transport",
        "injected fault at stdin_transport",
        &["--stdin"],
        false,
    );
    assert_eq!(
        code,
        Some(5),
        "the waiting client gets the terminal snapshot"
    );
}

#[test]
fn running_meta_failure_stops_the_child_and_records_sidecar_failed() {
    let code = ends_the_run(
        Inject::Fail("running_meta"),
        "running_meta",
        "injected fault at running_meta",
        &[],
        false,
    );
    assert_eq!(code, Some(5));
}

#[test]
fn child_wait_failure_stops_the_child_and_records_sidecar_failed() {
    let code = ends_the_run(
        Inject::Fail("child_wait"),
        "child_wait",
        "cannot observe the child's exit: injected fault at child_wait",
        &[],
        true,
    );
    assert_eq!(code, Some(0), "start had already returned Running");
}

#[test]
fn panic_before_running_is_recorded_with_its_step() {
    let code = ends_the_run(
        Inject::Panic("running_meta"),
        "running_meta",
        "panicked: ",
        &[],
        false,
    );
    assert_eq!(code, Some(5));
}

#[test]
fn panic_while_running_is_recorded_with_its_step() {
    let code = ends_the_run(
        Inject::Panic("output_log"),
        "output_log",
        "injected panic at output_log",
        &[],
        false,
    );
    assert_eq!(code, Some(0));
}

/// The group kill reaches a PTY child, which is a session leader (`setsid`),
/// and the rest of its process group.
#[cfg(unix)]
#[test]
fn pty_failure_stops_the_session_leader_and_its_group() {
    let code = ends_the_run(
        Inject::Fail("running_meta"),
        "running_meta",
        "injected fault at running_meta",
        &["--pty"],
        false,
    );
    assert_eq!(code, Some(5));
}

#[cfg(unix)]
#[test]
fn pty_stdin_transport_failure_stops_the_session_leader_and_its_group() {
    let code = ends_the_run(
        Inject::Fail("stdin_transport"),
        "stdin_transport",
        "injected fault at stdin_transport",
        &["--pty", "--stdin"],
        false,
    );
    assert_eq!(code, Some(5));
}

// === Recording the failure itself fails ===

/// The child is still stopped, and the failure is reported on the channels
/// that remain: an `ERROR:` to the waiting client and a lost+found copy. No
/// terminal record is claimed, and the breadcrumb stays for recovery.
#[test]
fn failure_record_failure_still_stops_the_child_and_says_so() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let gate = root.path().join("fault-gate");
    let client = spawn_start(
        &root,
        Inject::Fail("stdin_transport,failure_record"),
        &gate,
        &["s1", "--stdin", "--", "sleep", "60"],
    );
    let breadcrumb = session_dir(&root, "s1").join("child_pid");
    wait_for_file(&breadcrumb, "the child to be spawned");
    std::fs::write(&gate, b"").unwrap();

    let (code, _stdout, stderr) = finish_client(client);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("sidecar failed at stdin_transport")
            && stderr.contains("recording the failure also failed"),
        "{stderr}"
    );

    let (meta_path, identity) = {
        let dir = session_dir(&root, "s1");
        let identity: ProcessIdentity =
            serde_json::from_str(&std::fs::read_to_string(dir.join("child_pid")).unwrap()).unwrap();
        (dir.join("meta.json"), identity)
    };
    assert_process_gone(&identity, "the child");
    assert!(
        !meta_path.exists(),
        "Running was never published and the failure was not recorded"
    );

    let lost = std::fs::read_to_string(root.path().join(".tender/lost+found/events.jsonl"))
        .expect("the failure is salvaged to lost+found");
    let record: serde_json::Value = lost
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|e| e["kind"] == "run.sidecar_failed")
        .unwrap_or_else(|| panic!("no run.sidecar_failed in lost+found: {lost}"));
    assert_eq!(record["data"]["step"], "stdin_transport");
    assert!(
        record["data"]["meta_write_error"]
            .as_str()
            .unwrap()
            .contains("injected fault at failure_record")
    );
}

// === Steps that recover with a visible warning ===

/// Run `-- <argv>` with `inject`, and return the terminal meta once the
/// sidecar is gone.
fn recovers(inject: Inject, flags: &[&str], argv: &[&str]) -> (TempDir, serde_json::Value) {
    let root = TempDir::new().unwrap();
    let gate = root.path().join("fault-gate");
    std::fs::write(&gate, b"").unwrap();
    let mut args: Vec<&str> = vec!["s1"];
    args.extend_from_slice(flags);
    args.push("--");
    args.extend_from_slice(argv);
    let client = spawn_start(&root, inject, &gate, &args);
    let _ = finish_client(client);
    let (meta, lock) = wait_sidecar_gone(&root, "s1");
    drop(lock);
    (root, meta)
}

fn assert_exited_ok(meta: &serde_json::Value) {
    assert_eq!(meta["status"], "Exited", "{meta}");
    assert_eq!(meta["reason"], "ExitedOk", "{meta}");
}

#[test]
fn breadcrumb_failure_recovers_with_a_warning() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_root, meta) = recovers(Inject::Fail("breadcrumb"), &[], &["true"]);
    assert_exited_ok(&meta);
    assert_warning(
        &meta,
        "orphan breadcrumb not written: injected fault at breadcrumb",
    );
}

/// Readiness lost and the meta rewrite that records it failing too: the
/// warning rides the next meta write instead of ending the run.
#[test]
fn readiness_and_its_rewrite_failing_recover_with_a_warning() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_root, meta) = recovers(Inject::Fail("readiness,ready_rewrite"), &[], &["true"]);
    assert_exited_ok(&meta);
    assert_warning(&meta, "readiness not delivered: start client gone");
}

/// Without `output.log` the child's output is drained, so a child writing far
/// more than a pipe buffer still runs to its real exit.
#[test]
fn output_log_failure_drains_output_and_recovers() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (root, meta) = recovers(
        Inject::Fail("output_log"),
        &[],
        &[
            "sh",
            "-c",
            "yes tender | head -c 2000000; yes tender | head -c 2000000 >&2",
        ],
    );
    assert_exited_ok(&meta);
    assert_warning(&meta, "output not captured: cannot open output.log");
    let log = session_dir(&root, "s1").join("output.log");
    assert!(
        std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0) == 0,
        "nothing was written to output.log"
    );
}

#[cfg(unix)]
#[test]
fn attach_bind_failure_recovers_with_a_warning() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (root, meta) = recovers(Inject::Fail("attach_bind"), &["--pty"], &["true"]);
    assert_exited_ok(&meta);
    assert_warning(&meta, "attach unavailable: injected fault at attach_bind");
    assert!(!session_dir(&root, "s1").join("a.sock.path").exists());
}

/// The child exited, so there is nothing to stop: the durable event lets
/// reconciliation heal meta, a copy goes to lost+found, and hooks still run.
#[test]
fn terminal_meta_failure_still_runs_hooks_and_heals() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let marker = root.path().join("hook-ran");
    let hook = touch_cmd(&marker);
    let gate = root.path().join("fault-gate");
    std::fs::write(&gate, b"").unwrap();
    let client = spawn_start(
        &root,
        Inject::Fail("terminal_meta"),
        &gate,
        &["s1", "--on-exit", &hook, "--", "true"],
    );
    let _ = finish_client(client);
    let (on_disk, lock) = wait_sidecar_gone(&root, "s1");
    assert_eq!(
        on_disk["status"], "Running",
        "terminal meta was not written"
    );
    drop(lock);
    wait_for_file(&marker, "the --on-exit hook");

    assert!(event_kinds(&root, "s1").iter().any(|k| k == "run.exited"));
    let output = tender(&root).args(["status", "s1"]).output().unwrap();
    let healed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_exited_ok(&healed);
    assert_eq!(
        healed["transition_provenance"]["evidence"],
        serde_json::json!(["event_log_terminal"])
    );

    let lost = std::fs::read_to_string(root.path().join(".tender/lost+found/events.jsonl"))
        .expect("a copy of the terminal record is salvaged");
    assert!(lost.contains("meta_write_error"), "{lost}");
}

// === Platform premises ===

/// #74's premise, per platform: writing readiness to a pipe whose reader has
/// gone fails (EPIPE on Unix; on Windows a closed pipe), rather than blocking
/// or succeeding silently.
#[test]
fn readiness_write_to_a_closed_reader_fails() {
    let (reader, writer) = Current::ready_channel().expect("ready channel");
    drop(reader);
    let result = Current::write_ready_signal(writer, "OK:{}\n");
    assert!(
        result.is_err(),
        "write to a closed readiness pipe succeeded"
    );
}

/// The platform kill path the guard uses (`kill_child`, here driven by
/// `--timeout`) reaches a PTY child, which is a session leader, and its group.
#[cfg(unix)]
#[test]
fn pty_group_kill_reaches_the_session_leader_and_its_children() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let (argv, gpid) = family_child(root.path(), false);
    let mut args = vec!["start", "s1", "--pty", "--timeout", "1", "--"];
    args.extend(argv.iter().map(String::as_str));
    tender(&root).args(&args).assert().success();
    wait_for_file(&gpid, "the grandchild to start");

    let (meta, _lock) = wait_sidecar_gone(&root, "s1");
    assert_eq!(meta["reason"], "TimedOut", "{meta}");
    assert_process_gone(&child_identity(&meta), "the PTY session leader");
    assert_pid_gone(&gpid, "the session leader's child");
}
