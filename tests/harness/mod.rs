#[allow(unused_imports)]
use assert_cmd::Command;
use assert_cmd::assert::{Assert, OutputAssertExt};
#[allow(unused_imports)]
use std::path::Path;
use std::time::{Duration, Instant};
#[allow(unused_imports)]
use tempfile::TempDir;
use tender::model::ids::{Namespace, SessionName};
use tender::session::{self, LockGuard, SessionRoot};

/// Hang-detector deadline for a single `tender` CLI invocation in tests.
///
/// This is NOT a performance assertion. A normal detached start completes in
/// ~20 ms (warm) to ~570 ms (cold); this bound exists only to fail a genuine
/// hang (deadlock) rather than let it stall CI forever. It is deliberately
/// generous: the old hard-coded 5 s per-command deadline false-fired on a
/// loaded Windows CI runner — `assert_cmd` killed the still-starting `tender`
/// process, which surfaced as exit 1 with empty output. 30 s keeps substantial
/// headroom over observed individual starts while still surfacing a true hang
/// promptly. Fixed and shared — raise this one value only if a real environment
/// proves it insufficient.
#[allow(dead_code)]
pub const CMD_DEADLINE: Duration = Duration::from_secs(30);

/// Extension trait so call sites read `cmd.args(..).assert_within_deadline()` —
/// a drop-in for the fluent `.assert()` that runs the command under the shared
/// [`CMD_DEADLINE`] hang detector with explicit timeout diagnostics.
#[allow(dead_code)]
pub trait DeadlineAssertExt {
    /// Assert the command under the shared [`CMD_DEADLINE`] hang detector.
    fn assert_within_deadline(&mut self) -> Assert;
}

impl DeadlineAssertExt for Command {
    fn assert_within_deadline(&mut self) -> Assert {
        assert_within(self, CMD_DEADLINE)
    }
}

/// Like [`assert_within_deadline`] but with an explicit deadline (used by the
/// deadline's own regression test). If the invocation returns at or beyond
/// `deadline`, this panics with an explicit message naming the command and the
/// deadline. `assert_cmd` normally enforces that bound by killing an overrun,
/// but the diagnostic intentionally states only the wall-clock fact we can
/// observe rather than inferring how the process ended.
#[allow(dead_code)]
pub fn assert_within(cmd: &mut Command, deadline: Duration) -> Assert {
    let desc = format!("{cmd:?}");
    let start = Instant::now();
    let outcome = cmd.timeout(deadline).output();
    let elapsed = start.elapsed();
    if elapsed >= deadline {
        panic!(
            "HARNESS TIMEOUT: command exceeded the {:.1}s harness deadline; the invocation \
             returned after {:.1}s.\n  command: {desc}\n  This is the harness hang-detector \
             firing — on a loaded runner it may mean the process was starved, not a product failure. \
             Raise harness::CMD_DEADLINE only after ruling out a real hang.",
            deadline.as_secs_f64(),
            elapsed.as_secs_f64(),
        );
    }
    outcome
        .expect("failed to launch command (non-timeout spawn error)")
        .assert()
}

/// Create a `tender` command rooted in a temp HOME.
#[allow(dead_code)]
pub fn tender(root: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("tender").expect("tender binary not found");
    cmd.env("HOME", root.path());
    // On Windows, ensure Git-for-Windows coreutils (echo, sleep, true, cat)
    // are on PATH so tests can spawn Unix-style commands.
    #[cfg(windows)]
    {
        let git_usr_bin = std::path::Path::new(r"C:\Program Files\Git\usr\bin");
        if git_usr_bin.exists() {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{};{path}", git_usr_bin.display()));
        }
    }
    cmd
}

/// Path to the `test_callback` fixture binary (built by cargo as a sibling of the test binary).
#[allow(dead_code)]
pub fn test_callback_bin() -> String {
    let bin = assert_cmd::cargo::cargo_bin("test_callback");
    bin.to_str()
        .expect("test_callback path is valid UTF-8")
        .to_owned()
}

/// `test_callback_bin()` quoted for embedding in on-exit command strings
/// that will be parsed by `shell_words::split`.
#[allow(dead_code)]
fn test_callback_bin_quoted() -> String {
    shell_words::quote(&test_callback_bin()).into_owned()
}

/// Return an on-exit command string that creates `path` as an empty marker file.
/// Parsed by `shell_words::split` in the sidecar, then executed directly — no shell involved.
#[allow(dead_code)]
pub fn touch_cmd(path: &Path) -> String {
    let quoted = shell_words::quote(path.to_str().expect("path is valid UTF-8"));
    format!("{} touch {quoted}", test_callback_bin_quoted())
}

/// Return an on-exit command string that writes TENDER_SESSION, TENDER_NAMESPACE,
/// and TENDER_EXIT_REASON to the given file.
/// Parsed by `shell_words::split` in the sidecar, then executed directly — no shell involved.
#[allow(dead_code)]
pub fn echo_env_cmd(path: &Path) -> String {
    let quoted = shell_words::quote(path.to_str().expect("path is valid UTF-8"));
    format!("{} echo-env {quoted}", test_callback_bin_quoted())
}

/// Read all stored events for a default-namespace session, merged by
/// (ts, writer, seq) — the spec §4 merge rule.
#[allow(dead_code)]
pub fn read_events(root: &TempDir, session: &str) -> Vec<serde_json::Value> {
    let events_dir = root
        .path()
        .join(format!(".tender/sessions/default/{session}/events"));
    let mut segments: Vec<_> = std::fs::read_dir(&events_dir)
        .expect("events dir exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segments.sort();

    let mut events: Vec<serde_json::Value> = Vec::new();
    for seg in segments {
        for line in std::fs::read_to_string(&seg).unwrap().lines() {
            if !line.is_empty() {
                events.push(serde_json::from_str(line).expect("event line parses"));
            }
        }
    }
    events.sort_by_key(|e| {
        (
            e["ts"].as_str().unwrap().to_owned(),
            e["writer"].as_str().unwrap().to_owned(),
            e["seq"].as_u64().unwrap(),
        )
    });
    events
}

/// Wait for a session event of `kind`, returning the observed record.
#[allow(dead_code)]
pub fn wait_event_kind(root: &TempDir, session: &str, kind: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = read_events(root, session)
            .into_iter()
            .find(|event| event["kind"] == kind)
        {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for event kind {kind} in {session}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Wait for meta.json to show Running state on disk.
#[allow(dead_code)]
pub fn wait_running(root: &TempDir, session: &str) {
    let path = root
        .path()
        .join(format!(".tender/sessions/default/{session}/meta.json"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
                if meta["status"].as_str() == Some("Running") {
                    return;
                }
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for Running state in {session}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Wait for meta.json to reach any terminal state on disk.
#[allow(dead_code)]
pub fn wait_terminal(root: &TempDir, session: &str) -> serde_json::Value {
    let path = root
        .path()
        .join(format!(".tender/sessions/default/{session}/meta.json"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
                let status = meta["status"].as_str().unwrap_or("");
                if status != "Starting" && status != "Running" {
                    return meta;
                }
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for terminal state in {session}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Proof that a session is terminal and its sidecar has released ownership.
///
/// Construction is private: callers can only obtain this guard through
/// [`wait_terminal_quiescent`], which observes terminal metadata and then
/// acquires the session lock. Keeping the lock guard alive prevents another
/// owner from invalidating that observation while the caller asserts against
/// the quiescent session.
#[allow(dead_code)]
pub struct QuiescentTerminal {
    _lock: LockGuard,
}

/// Wait until terminal metadata is durable and the sidecar lock is available.
///
/// Terminal metadata alone does not prove that the sidecar has exited: the
/// sidecar writes its final state immediately before releasing the lock. This
/// condition handshake acquires the lock instead of relying on a timing gap.
#[allow(dead_code)]
pub fn wait_terminal_quiescent(root: &TempDir, session_name: &str) -> QuiescentTerminal {
    let session_root = SessionRoot::new(root.path().join(".tender/sessions"));
    let namespace = Namespace::new("default").expect("default namespace is valid");
    let session_name = SessionName::new(session_name).expect("test session name is valid");
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        if let Ok(Some(session_dir)) = session::open(&session_root, &namespace, &session_name) {
            if session::read_meta(&session_dir).is_ok_and(|meta| meta.status().is_terminal()) {
                match LockGuard::try_acquire(&session_dir) {
                    Ok(lock) => return QuiescentTerminal { _lock: lock },
                    Err(session::SessionError::Locked(_)) => {}
                    Err(error) => panic!("failed to acquire terminal session lock: {error}"),
                }
            }
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for terminal session {session_name} to become quiescent"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Gate a test on the external `duckdb` CLI. Returns `true` if the test should
/// proceed. When duckdb is absent:
///   - panics if `TENDER_REQUIRE_DUCKDB_TESTS` is set (CI installs duckdb, so a
///     broken install must fail loudly rather than silently pass as green);
///   - otherwise prints a skip notice and returns `false` so the caller returns
///     early (friendly local runs without duckdb installed).
#[allow(dead_code)]
pub fn duckdb_or_skip() -> bool {
    let available = std::process::Command::new("duckdb")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !available {
        if std::env::var_os("TENDER_REQUIRE_DUCKDB_TESTS").is_some() {
            panic!(
                "duckdb not found on PATH but TENDER_REQUIRE_DUCKDB_TESTS is set \
                 -- CI must install the DuckDB CLI"
            );
        }
        eprintln!("skipped: duckdb not on PATH (set TENDER_REQUIRE_DUCKDB_TESTS to enforce)");
    }
    available
}
