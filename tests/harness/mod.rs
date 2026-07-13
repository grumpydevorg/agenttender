#[allow(unused_imports)]
use assert_cmd::Command;
use assert_cmd::assert::{Assert, OutputAssertExt};
#[allow(unused_imports)]
use std::io::{BufRead, BufReader, Read};
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[allow(unused_imports)]
use std::process::{Child, Stdio};
#[allow(unused_imports)]
use std::sync::atomic::{AtomicU64, Ordering};
#[allow(unused_imports)]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[allow(unused_imports)]
use tempfile::TempDir;
use tender::model::ids::{Namespace, SessionName};
use tender::model::meta::Meta;
use tender::model::state::RunStatus;
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

/// Proof that a session is the residue of an intentionally-crashed sidecar:
/// `Running + Unlocked`. The sidecar is gone (its lock was acquired) and the
/// metadata it left behind is still non-terminal `Running`.
///
/// Construction is private: callers can only obtain this through
/// [`wait_orphaned_running`], which acquires the session lock — proving the
/// aborted sidecar exited — and then verifies the status. Both fields are
/// retained to make the proof-carrying state explicit even though the caller
/// needs neither; holding the guard keeps a new sidecar from taking the session
/// while the caller inspects meta and events.
#[allow(dead_code)]
pub struct OrphanedRunning {
    _meta: Meta,
    _lock: LockGuard,
}

/// Wait until an intentionally-crashed sidecar's residue is observable —
/// acquiring the session lock is the proof of death, so no fixed sleep.
///
/// Only lock *contention* is retried (the sidecar has not exited yet). Every
/// other condition fails immediately, so a real defect surfaces as a clear
/// message rather than a timeout: a missing or unreadable session, any non-
/// contention lock error, or a terminal status once the lock is held —
/// terminal-plus-acquired-lock is a stable contradiction for this injected crash
/// point, not a transient state worth polling further.
#[allow(dead_code)]
pub fn wait_orphaned_running(root: &TempDir, session_name: &str) -> OrphanedRunning {
    let session_root = SessionRoot::new(root.path().join(".tender/sessions"));
    let namespace = Namespace::new("default").expect("default namespace is valid");
    let session_name = SessionName::new(session_name).expect("test session name is valid");
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        // `start` has returned, so the session directory and meta must exist:
        // anything else is a defect, not a state to wait out.
        let session_dir = match session::open(&session_root, &namespace, &session_name) {
            Ok(Some(dir)) => dir,
            Ok(None) => panic!("crashed session {session_name} does not exist"),
            Err(error) => panic!("failed to open crashed session {session_name}: {error}"),
        };

        match LockGuard::try_acquire(&session_dir) {
            Ok(lock) => {
                // The lock is ours, so the aborted sidecar is gone. What it left
                // behind must still be Running: a terminal status here means the
                // crash did not abort before finalization.
                let meta = session::read_meta(&session_dir)
                    .expect("crashed session must have readable meta");
                assert!(
                    matches!(meta.status(), RunStatus::Running { .. }),
                    "invariant violation: sidecar lock released but status is {:?}, expected \
                     Running — the injected crash did not leave a Running + Unlocked orphan",
                    meta.status()
                );
                return OrphanedRunning {
                    _meta: meta,
                    _lock: lock,
                };
            }
            // The only retryable condition: the sidecar has not exited yet.
            Err(session::SessionError::Locked(_)) => {}
            Err(error) => panic!("failed to acquire crashed session lock: {error}"),
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for the crashed sidecar to release the lock on {session_name}"
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

/// Distinguishes ready-file paths when a test spawns several followers.
#[allow(dead_code)]
static FOLLOWER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Poll until `path` exists or `deadline` elapses (panicking). The follower
/// publishes its `--ready-file` only after its baseline is established, so this
/// is the deterministic "safe to mutate now" barrier that replaces a fixed
/// post-spawn sleep.
#[allow(dead_code)]
pub fn wait_ready_file(path: &Path, deadline: Duration) {
    let end = Instant::now() + deadline;
    while !path.exists() {
        assert!(
            Instant::now() < end,
            "timed out waiting for ready-file {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// An opaque handle to a running follower (`tender events --follow` or
/// `tender watch`) whose initial baseline is *proven* established: [`spawn`]
/// returns only after the follower publishes its `--ready-file`. Holding one
/// means "safe to perform live mutations now."
///
/// [`read_until`](Self::read_until) waits for a specific streamed record with a
/// deadline; [`stop`](Self::stop) consumes the handle and reaps the child; and
/// `Drop` reaps the child even on panic, so a failing test never leaks a
/// follower process.
///
/// [`spawn`]: Self::spawn
#[allow(dead_code)]
pub struct ReadyFollower {
    child: Child,
    label: String,
    records: Arc<Mutex<Vec<serde_json::Value>>>,
    stdout_reader: Option<std::thread::JoinHandle<()>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
}

#[allow(dead_code)]
impl ReadyFollower {
    /// Spawn `tender <subcommand> <args...> --ready-file <temp>` and block until
    /// the follower signals readiness. Panics — after killing/reaping the child
    /// — if the follower exits before readiness or the 10 s barrier elapses,
    /// surfacing the follower's captured stderr rather than a bare timeout.
    pub fn spawn(root: &TempDir, subcommand: &str, args: &[&str]) -> Self {
        let bin = assert_cmd::cargo::cargo_bin("tender");
        let seq = FOLLOWER_SEQ.fetch_add(1, Ordering::Relaxed);
        let ready_path = root.path().join(format!("follower-ready-{seq}"));
        let label = format!("tender {subcommand} {}", args.join(" "));

        let mut child = std::process::Command::new(bin)
            .arg(subcommand)
            .args(args)
            .arg("--ready-file")
            .arg(&ready_path)
            .env("HOME", root.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn follower `{label}`: {e}"));

        // Drain stdout AND stderr immediately — before waiting for readiness.
        // Production flushes the initial replay/snapshot *before* creating the
        // ready-file, so a replay larger than the OS pipe buffer would deadlock
        // if we waited first: the follower blocks flushing stdout and never
        // reaches the ready-file. Records accumulated here (the replay) are
        // preserved for later read_until calls.
        let records = Arc::new(Mutex::new(Vec::new()));
        let mut stdout_reader = {
            let records = Arc::clone(&records);
            let stdout = child.stdout.take().expect("piped stdout");
            Some(std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        records.lock().unwrap().push(v);
                    }
                }
            }))
        };
        // stderr is drained so a premature exit is reported with its diagnostics
        // and a chatty stderr can't wedge on a full pipe.
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let mut stderr_reader = {
            let buf = Arc::clone(&stderr_buf);
            let mut err = child.stderr.take().expect("piped stderr");
            Some(std::thread::spawn(move || {
                let mut s = String::new();
                let _ = err.read_to_string(&mut s);
                *buf.lock().unwrap() = s;
            }))
        };

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if ready_path.exists() {
                break;
            }
            // On failure, reap the child and join both readers so the captured
            // stderr is complete (the pipes EOF once the child is gone), instead
            // of sampling it after a fixed sleep.
            if let Ok(Some(status)) = child.try_wait() {
                if let Some(h) = stdout_reader.take() {
                    let _ = h.join();
                }
                if let Some(h) = stderr_reader.take() {
                    let _ = h.join();
                }
                let stderr = stderr_buf.lock().unwrap().clone();
                panic!("follower `{label}` exited before readiness ({status}); stderr:\n{stderr}");
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(h) = stdout_reader.take() {
                    let _ = h.join();
                }
                if let Some(h) = stderr_reader.take() {
                    let _ = h.join();
                }
                let stderr = stderr_buf.lock().unwrap().clone();
                panic!(
                    "follower `{label}` never published its ready-file within 10s; stderr:\n{stderr}"
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        ReadyFollower {
            child,
            label,
            records,
            stdout_reader,
            stderr_reader,
        }
    }

    /// Block until a streamed record satisfies `pred`, returning it. Panics on
    /// `deadline`. Rescans every record seen so far each poll, so a record that
    /// arrived just before the call is never missed — this is the delivery
    /// barrier that replaces a fixed pre-kill sleep.
    pub fn read_until(
        &self,
        deadline: Duration,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let end = Instant::now() + deadline;
        loop {
            if let Some(found) = self
                .records
                .lock()
                .unwrap()
                .iter()
                .find(|r| pred(r))
                .cloned()
            {
                return found;
            }
            assert!(
                Instant::now() < end,
                "follower `{}` produced no matching record within {deadline:?}",
                self.label
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Snapshot of every record streamed so far (for negative assertions such as
    /// "history was skipped"). Take it only after a `read_until` has proven the
    /// stream reached the point you care about.
    pub fn records(&self) -> Vec<serde_json::Value> {
        self.records.lock().unwrap().clone()
    }

    /// Kill and reap the follower, consuming the handle.
    pub fn stop(mut self) {
        self.terminate();
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.stdout_reader.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_reader.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ReadyFollower {
    fn drop(&mut self) {
        self.terminate();
    }
}
