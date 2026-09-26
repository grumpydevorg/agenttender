//! The per-session sidecar — the lifecycle authority.
//!
//! Spawned detached by `start`, the sidecar holds the session lock, spawns the
//! supervised child via the [`platform`](crate::platform) backend, writes run
//! [`state`](crate::model::state) transitions into
//! [`Meta`], captures the child's output into the
//! append-only log, watches for kill requests, and classifies the exit. The
//! CLI normally only *asks*; after the sidecar is gone, the narrowly scoped
//! [`reconcile`](crate::reconcile) path may heal or infer terminal state.

use std::io;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;

mod supervised_run;
use supervised_run::SupervisedRun;

use crate::events::{self, EventDraft, EventWriter};
use crate::model::dep_fail::DepFailReason;
use crate::model::event::{Kind, Uuid7};
use crate::model::ids::{EpochTimestamp, Generation, Namespace, RunId, SessionName, Source};
use crate::model::meta::Meta;
#[cfg(unix)]
use crate::model::pty::PtyControl;
#[cfg(all(test, unix))]
use crate::model::pty::PtyMeta;
use crate::model::spec::{DependencyBinding, IoMode, LaunchSpec};
use crate::model::state::{ExitReason, RunStatus, SidecarStep};
use crate::platform::{Current, Platform};
use crate::session::{self, LockGuard, SessionDir, SessionRoot};

/// Type alias for the platform's ReadyWriter to avoid verbose turbofish.
type ReadyWriter = <Current as Platform>::ReadyWriter;

/// Shared sink for teeing PTY output to an attached client.
type AttachSink = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// Run the sidecar process. Called from the `_sidecar` subcommand.
///
/// Contract:
/// - Acquire session lock
/// - Read launch spec from session dir
/// - Spawn child process (write child_pid breadcrumb immediately)
/// - Write meta.json with Running state (or SpawnFailed)
/// - Send meta JSON snapshot over ready pipe (no race with disk state)
/// - Capture child stdout/stderr to output.log with timestamps
/// - Write terminal state when child exits
/// - Release lock and exit
pub fn run(session_dir: PathBuf, ready_writer: ReadyWriter) -> anyhow::Result<()> {
    remember_panics();

    // Wrap so we can track whether it's been consumed.
    // write_ready_signal takes ownership -- Option prevents double-use.
    // After spawn the lifecycle guard owns it, and answers the client itself.
    let mut ready = Some(ready_writer);

    let result = run_inner(&session_dir, &mut ready);

    if let Err(ref e) = result {
        // Only signal error if the file hasn't been consumed yet
        if let Some(file) = ready.take() {
            let _ = Current::write_ready_signal(file, &format!("ERROR:{e}\n"));
        }
    }

    result
}

/// The most recent panic message in this process, for the lifecycle guard's
/// record: a guard dropped by an unwinding panic cannot see the payload.
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Keep each panic's message (then run the default hook; sidecar stderr is
/// /dev/null, so without this the reason would be lost).
fn remember_panics() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut slot) = LAST_PANIC.lock() {
            *slot = Some(info.to_string());
        }
        default_hook(info);
    }));
}

fn last_panic_message() -> String {
    LAST_PANIC
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

/// Create the stdin transport and spawn a forwarding thread.
/// The transport is moved into the forwarding thread (it needs the server-side
/// handle on Windows). Cleanup is handled by `remove_stdin_transport`.
fn setup_stdin_forwarding(
    session_dir: &Path,
    child_stdin: Box<dyn Write + Send>,
    stdin_errors: &Arc<Mutex<Vec<String>>>,
) -> io::Result<()> {
    // StdinTransport is () on Unix — clippy flags the let-binding but
    // forward_stdin needs the value on Windows where the type is non-unit.
    #[allow(clippy::let_unit_value)]
    let transport = Current::create_stdin_transport(session_dir)?;

    // Spawn forwarding thread (detached -- not joined).
    // Thread owns the transport and exits when child stdin breaks or transport is removed.
    let session_dir_clone = session_dir.to_path_buf();
    let errors_clone = Arc::clone(stdin_errors);
    std::thread::spawn(move || {
        forward_stdin(transport, session_dir_clone, child_stdin, errors_clone)
    });

    Ok(())
}

/// Spawn a timeout thread that kills the child after `timeout_s` seconds.
/// Returns the `timed_out` flag. The caller passes a `cancel` flag to prevent the kill
/// after the child exits naturally.
///
/// Takes a `ChildKillHandle` (lightweight, Send + Clone) extracted from the
/// SupervisedChild, so the timeout thread uses the live backend context
/// (Job Object on Windows, process group on Unix) rather than degrading
/// to orphan-kill semantics.
fn setup_timeout(
    kill_handle: <Current as Platform>::ChildKillHandle,
    timeout_s: u64,
    cancel: Arc<AtomicBool>,
) -> Arc<AtomicBool> {
    let timed_out = Arc::new(AtomicBool::new(false));
    let timed_out_clone = Arc::clone(&timed_out);
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_s);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return; // Child exited before timeout -- don't kill
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if cancel.load(Ordering::Relaxed) {
            return; // Final check after deadline
        }
        timed_out_clone.store(true, Ordering::Relaxed);
        // Use kill_child with the live kill handle context.
        // On Windows this uses the Job Object for full tree kill;
        // on Unix this uses process group kill.
        let _ = Current::kill_child(&kill_handle, true);
    });
    timed_out
}

/// Spawn a thread that watches for a `kill_request` file from the CLI.
/// When found, validates the run_id matches (preventing stale requests from
/// killing a replacement run), then calls kill_child with the live
/// ChildKillHandle for tree-aware kill.
fn setup_kill_watcher(
    session_dir: &Path,
    kill_handle: <Current as Platform>::ChildKillHandle,
    run_id: RunId,
    cancel: Arc<AtomicBool>,
) {
    let kill_request_path = session_dir.join("kill_request");
    let kill_acted_path = session_dir.join("kill_acted");
    let run_id_str = run_id.to_string();
    std::thread::spawn(move || {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if kill_request_path.exists() {
                // Parse the request. If unreadable or malformed, discard it
                // rather than defaulting to force (avoids partial-read upgrades).
                let parsed = std::fs::read_to_string(&kill_request_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

                // Always clean up the request file.
                let _ = std::fs::remove_file(&kill_request_path);

                let request = match parsed {
                    Some(v) => v,
                    None => continue, // Malformed — ignore, CLI will retry or fall back
                };

                // Validate run_id — reject stale requests from a previous run.
                if request["run_id"].as_str() != Some(&run_id_str) {
                    continue; // Wrong run — ignore
                }

                let force = request["force"].as_bool().unwrap_or(false);

                // Leave a breadcrumb so exit classification knows this was
                // a sidecar-mediated kill (not a spontaneous child exit).
                // The kill_forced marker handles force=true classification;
                // this breadcrumb handles force=false → Killed.
                if !force {
                    let _ = std::fs::write(&kill_acted_path, "");
                }

                // Use the live kill handle for tree-aware kill.
                let _ = Current::kill_child(&kill_handle, force);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}

/// Collect capture errors and stdin forwarding errors into a warning list.
fn collect_warnings(session_dir: &Path, stdin_errors: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    let mut warnings = Vec::new();

    // Collect capture errors
    let capture_err_path = session_dir.join("capture_errors.log");
    if let Ok(errors) = std::fs::read_to_string(&capture_err_path) {
        for line in errors.lines() {
            if !line.is_empty() {
                warnings.push(format!("log capture: {line}"));
            }
        }
    }

    // Collect stdin forwarding errors
    if let Ok(errs) = stdin_errors.lock() {
        for e in errs.iter() {
            warnings.push(e.clone());
        }
    }

    warnings
}

/// Outcome of the dependency wait phase.
enum DepWaitOutcome {
    /// All dependencies satisfied — proceed to spawn.
    Satisfied,
    /// A dependency failed (non-zero exit, not found, replaced).
    Failed(String),
    /// Timeout expired during the wait.
    TimedOut(String),
    /// Graceful kill request received during the wait.
    Killed(String),
    /// Force kill request received during the wait.
    KilledForced(String),
}

/// A `--after` dependency after the first scan. Only `Pending` and `Latched`
/// coexist with continued polling; failure/timeout/kill are phase-terminal
/// ([`FirstScanOutcome::Terminal`]) and are never represented here.
enum ScannedDependency {
    Pending(DependencyBinding),
    /// Satisfied at its bound run_id and never re-examined — the binding is not
    /// retained because a latched dependency is never inspected again.
    Latched,
}

/// The dependency set produced by [`first_dependency_scan`]. Its private field
/// makes that scan the only producer, so accepting a `ScannedDependencies` (as
/// [`poll_dependencies`] does) is a static proof that a first scan has run.
struct ScannedDependencies(Vec<ScannedDependency>);

/// Exhaustive outcome of the first complete dependency scan.
enum FirstScanOutcome {
    /// Every dependency already satisfied — spawn immediately.
    ReadyToSpawn,
    /// Some pending, none failed — publish `Starting` and enter the poll loop.
    Waiting(ScannedDependencies),
    /// The scan produced, or was pre-empted by, a phase-terminal outcome.
    Terminal {
        reason: DepFailReason,
        warning: String,
    },
}

/// What `run_inner` does after the single post-scan readiness publication.
enum DepAction {
    Spawn,
    Wait(ScannedDependencies),
    ReturnTerminal,
}

/// Result of one dependency-scan iteration over the not-yet-latched set.
enum ScanStep {
    AllLatched,
    StillPending,
    Terminal(DepFailReason, String),
}

/// One dependency-scan iteration: honour a targeted kill request, then the
/// timeout, then inspect every not-yet-latched dependency — latching a
/// newly-satisfied one, or reporting the first phase-terminal condition. Shared
/// by the first scan and the poll loop so their evaluation is identical.
fn scan_iteration(
    scanned: &mut ScannedDependencies,
    session_root: &SessionRoot,
    namespace: &Namespace,
    kill_request_path: &Path,
    run_id_str: &str,
    deadline: Option<std::time::Instant>,
    after_any_exit: bool,
) -> ScanStep {
    // A kill request targeted at this run pre-empts the wait.
    if kill_request_path.exists() {
        if let Ok(content) = std::fs::read_to_string(kill_request_path) {
            if let Ok(req) = serde_json::from_str::<serde_json::Value>(&content) {
                if req["run_id"].as_str() == Some(run_id_str) {
                    let _ = std::fs::remove_file(kill_request_path);
                    let force = req["force"].as_bool().unwrap_or(false);
                    return if force {
                        ScanStep::Terminal(
                            DepFailReason::KilledForced,
                            "force-killed during dependency wait".into(),
                        )
                    } else {
                        ScanStep::Terminal(
                            DepFailReason::Killed,
                            "killed during dependency wait".into(),
                        )
                    };
                }
            }
        }
        // Wrong run_id or malformed — remove and ignore.
        let _ = std::fs::remove_file(kill_request_path);
    }

    if let Some(dl) = deadline {
        if std::time::Instant::now() >= dl {
            return ScanStep::Terminal(
                DepFailReason::TimedOut,
                "timeout expired during dependency wait".into(),
            );
        }
    }

    let mut all_latched = true;
    for slot in scanned.0.iter_mut() {
        let ScannedDependency::Pending(binding) = slot else {
            continue; // already latched — never re-polled
        };

        let dep_session = match session::open(session_root, namespace, &binding.session) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return ScanStep::Terminal(
                    DepFailReason::Failed,
                    format!("dependency session not found: {}", binding.session),
                );
            }
            Err(e) => {
                return ScanStep::Terminal(
                    DepFailReason::Failed,
                    format!("failed to open dependency {}: {e}", binding.session),
                );
            }
        };
        let dep_meta = match session::read_meta(&dep_session) {
            Ok(m) => m,
            Err(e) => {
                return ScanStep::Terminal(
                    DepFailReason::Failed,
                    format!("failed to read dependency {}: {e}", binding.session),
                );
            }
        };

        // Bound to run_id: replacing a still-pending dependency invalidates it.
        if dep_meta.run_id() != binding.run_id {
            return ScanStep::Terminal(
                DepFailReason::Failed,
                format!(
                    "dependency {} was replaced (bound run_id {}, found {})",
                    binding.session,
                    binding.run_id,
                    dep_meta.run_id()
                ),
            );
        }

        if dep_meta.status().is_terminal() {
            if !after_any_exit {
                use crate::model::state::{ExitReason as ER, RunStatus};
                match dep_meta.status() {
                    RunStatus::Exited {
                        how: ER::ExitedOk, ..
                    } => {} // satisfied
                    _ => {
                        return ScanStep::Terminal(
                            DepFailReason::Failed,
                            format!(
                                "dependency {} exited with non-success state",
                                binding.session
                            ),
                        );
                    }
                }
            }
            *slot = ScannedDependency::Latched;
        } else {
            all_latched = false;
        }
    }

    if all_latched {
        ScanStep::AllLatched
    } else {
        ScanStep::StillPending
    }
}

/// The first complete dependency scan — run once, before readiness is signalled.
/// Every already-satisfied dependency is latched to its observed run_id.
fn first_dependency_scan(
    session_root: &SessionRoot,
    namespace: &Namespace,
    spec: &LaunchSpec,
    deadline: Option<std::time::Instant>,
    session_dir: &Path,
    run_id: &RunId,
) -> FirstScanOutcome {
    let kill_request_path = session_dir.join("kill_request");
    let run_id_str = run_id.to_string();
    let mut scanned = ScannedDependencies(
        spec.after
            .iter()
            .cloned()
            .map(ScannedDependency::Pending)
            .collect(),
    );
    match scan_iteration(
        &mut scanned,
        session_root,
        namespace,
        &kill_request_path,
        &run_id_str,
        deadline,
        spec.after_any_exit,
    ) {
        ScanStep::AllLatched => FirstScanOutcome::ReadyToSpawn,
        ScanStep::StillPending => FirstScanOutcome::Waiting(scanned),
        ScanStep::Terminal(reason, warning) => FirstScanOutcome::Terminal { reason, warning },
    }
}

/// Continue polling an already-scanned dependency set until it resolves. Taking
/// a `ScannedDependencies` (only [`first_dependency_scan`] can build one) makes
/// polling uninspected dependencies unrepresentable.
fn poll_dependencies(
    mut scanned: ScannedDependencies,
    session_root: &SessionRoot,
    namespace: &Namespace,
    deadline: Option<std::time::Instant>,
    session_dir: &Path,
    run_id: &RunId,
    after_any_exit: bool,
) -> DepWaitOutcome {
    let kill_request_path = session_dir.join("kill_request");
    let run_id_str = run_id.to_string();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        match scan_iteration(
            &mut scanned,
            session_root,
            namespace,
            &kill_request_path,
            &run_id_str,
            deadline,
            after_any_exit,
        ) {
            ScanStep::AllLatched => return DepWaitOutcome::Satisfied,
            ScanStep::StillPending => continue,
            ScanStep::Terminal(reason, msg) => {
                return match reason {
                    DepFailReason::Failed => DepWaitOutcome::Failed(msg),
                    DepFailReason::TimedOut => DepWaitOutcome::TimedOut(msg),
                    DepFailReason::Killed => DepWaitOutcome::Killed(msg),
                    DepFailReason::KilledForced => DepWaitOutcome::KilledForced(msg),
                };
            }
        }
    }
}

/// Prepare the single readiness publication. Leaves `meta` as `Starting` for
/// `ReadyToSpawn`/`Waiting`; for `Terminal`, transitions, emits, and persists
/// `DependencyFailed` before returning, so the snapshot signalled next is
/// truthful. Takes `session` because the terminal arm writes meta to disk.
fn apply_first_scan_outcome(
    session: &SessionDir,
    meta: &mut Meta,
    lifecycle: &mut LifecycleEvents,
    outcome: FirstScanOutcome,
) -> anyhow::Result<DepAction> {
    match outcome {
        FirstScanOutcome::ReadyToSpawn => Ok(DepAction::Spawn),
        FirstScanOutcome::Waiting(scanned) => Ok(DepAction::Wait(scanned)),
        FirstScanOutcome::Terminal { reason, warning } => {
            meta.add_warning(warning);
            meta.transition_dependency_failed(EpochTimestamp::now(), reason)?;
            lifecycle.emit(meta, true);
            session::write_meta_atomic(session, meta)?;
            Ok(DepAction::ReturnTerminal)
        }
    }
}

/// Appends the sidecar's lifecycle events to the session event log.
/// Writer identity is the run id; `seq` is contiguous across the run.
///
/// WAL order (spec §3.6): call `emit` after the meta transition but BEFORE
/// `write_meta_atomic`, with `durable: true` for terminal transitions. This
/// is an ORDERING guarantee against the crash window — a sidecar that dies
/// between the two writes leaves the event, never the meta. It is not an
/// IO-failure guarantee: if the append itself fails, supervision continues
/// (meta stays the current-state authority), the failure is recorded as a
/// meta warning, and the record is salvaged to lost+found.
struct LifecycleEvents {
    session_dir: PathBuf,
    writer: EventWriter,
    namespace: Namespace,
    session: SessionName,
    run_id: RunId,
    generation: Generation,
}

impl LifecycleEvents {
    fn new(
        session_dir: &Path,
        namespace: &Namespace,
        session: &SessionName,
        run_id: RunId,
        generation: Generation,
    ) -> Self {
        Self {
            session_dir: session_dir.to_path_buf(),
            writer: EventWriter::with_writer(session_dir, Uuid7::from(run_id)),
            namespace: namespace.clone(),
            session: session.clone(),
            run_id,
            generation,
        }
    }

    /// Same session and run, freshly minted writer identity — for sidecar
    /// threads that append concurrently with the lifecycle writer (the
    /// attach listener). The protocol is multi-writer by design: each
    /// writer keeps its own contiguous `seq` chain (spec §1).
    #[cfg(unix)]
    fn with_fresh_writer(&self) -> Self {
        Self {
            session_dir: self.session_dir.clone(),
            writer: EventWriter::new(&self.session_dir),
            namespace: self.namespace.clone(),
            session: self.session.clone(),
            run_id: self.run_id,
            generation: self.generation,
        }
    }

    /// The lifecycle event for meta's CURRENT status.
    fn lifecycle_draft(&self, meta: &Meta) -> EventDraft {
        EventDraft {
            id: None,
            kind: events::lifecycle_kind(meta.status()),
            namespace: self.namespace.clone(),
            session: self.session.clone(),
            run_id: self.run_id,
            generation: Some(self.generation.as_u64()),
            source: Source::trusted("tendr.sidecar").expect("tendr.sidecar is grammatical"),
            block_id: None,
            parent_id: None,
            data: Some(events::lifecycle_data(
                meta.status(),
                "direct",
                meta.launch_spec().boundary.as_ref(),
            )),
            preview: None,
        }
    }

    /// Append the lifecycle event for meta's CURRENT status. Never fails the
    /// run: an append failure becomes a meta warning and the record is
    /// salvaged to lost+found — supervision must not die, and the history
    /// record must not silently vanish, because the event log is unwritable.
    fn emit(&mut self, meta: &mut Meta, durable: bool) {
        let draft = self.lifecycle_draft(meta);
        if let Err(e) = self.writer.append(draft.clone(), durable) {
            meta.add_warning(format!("event log append failed: {e}"));
            self.salvage_to_lost_found(draft);
        }
    }

    /// Meta could not be written after a terminal transition: keep a copy of
    /// the terminal record, with the write error, outside the session dir.
    fn salvage_unrecorded(&self, meta: &Meta, record_error: &str) {
        let mut draft = self.lifecycle_draft(meta);
        if let Some(data) = draft.data.as_mut() {
            data["meta_write_error"] = serde_json::Value::String(record_error.to_owned());
            data["warnings"] = serde_json::json!(meta.warnings());
        }
        self.salvage_to_lost_found(draft);
    }

    /// Append a non-lifecycle sidecar fact (`callback.finished`, spec §1)
    /// through the same writer, so `seq` stays contiguous after the
    /// terminal transition. Best-effort by design (plan slice 3): no
    /// salvage, no meta warning — these are not terminal transitions.
    fn append_fact(&mut self, kind: &str, data: serde_json::Value) {
        let Ok(kind) = Kind::new(kind) else {
            return;
        };
        let draft = EventDraft {
            id: None,
            kind,
            namespace: self.namespace.clone(),
            session: self.session.clone(),
            run_id: self.run_id,
            generation: Some(self.generation.as_u64()),
            source: Source::trusted("tendr.sidecar").expect("tendr.sidecar is grammatical"),
            block_id: None,
            parent_id: None,
            data: Some(data),
            preview: None,
        };
        let _ = self.writer.append(draft, false);
    }

    /// Last-resort preservation when the session's event log is unwritable:
    /// the fully-addressed record lands in `~/.tendr/lost+found/events.jsonl`
    /// (spec §7 machinery) instead of vanishing. Best-effort by design.
    fn salvage_to_lost_found(&self, draft: EventDraft) {
        let Some(tendr_root) = self
            .session_dir
            .ancestors()
            .find(|p| p.ends_with("sessions"))
            .and_then(Path::parent)
        else {
            return;
        };
        let event = events::stamp_orphan_event(draft);
        let _ = events::append_lost_found(tendr_root, &event);
    }
}

/// Test-only crash injection: `TENDR_TEST_ABORT=<point>` aborts the sidecar
/// there, a true crash that skips the lifecycle guard (WAL ordering, orphan
/// recovery). Points: `after_spawn`, `after_running`, `before_terminal_event`,
/// `before_terminal_meta`. Compiled into debug builds only; release sidecars
/// ignore the variable entirely.
fn test_abort_point(point: &str) {
    if cfg!(debug_assertions) && std::env::var("TENDR_TEST_ABORT").as_deref() == Ok(point) {
        std::process::abort();
    }
}

/// Test-only fault injection at a named sidecar step. `TENDR_TEST_FAIL` is a
/// comma-separated list of points that return an injected error;
/// `TENDR_TEST_PANIC` names points that panic. The points are the
/// [`SidecarStep`] wire names plus `ready_rewrite`, `terminal_meta` and
/// `failure_record`. If `TENDR_TEST_FAULT_GATE` names a file, a named point
/// first waits for it (bounded), so a test can let the child reach a known
/// state before the fault fires. Compiled into debug builds only; release
/// sidecars ignore all three variables entirely.
fn test_fault(point: &str) -> io::Result<()> {
    if !cfg!(debug_assertions) {
        return Ok(());
    }
    let names = |var: &str| {
        std::env::var(var)
            .map(|v| v.split(',').any(|p| p.trim() == point))
            .unwrap_or(false)
    };
    let panics = names("TENDR_TEST_PANIC");
    let fails = names("TENDR_TEST_FAIL");
    if panics || fails {
        wait_for_gate_file("TENDR_TEST_FAULT_GATE");
    }
    if panics {
        panic!("injected panic at {point}");
    }
    if fails {
        return Err(io::Error::other(format!("injected fault at {point}")));
    }
    Ok(())
}

/// Wait until the file named by env var `var` exists, if it is set. Bounded,
/// so a failed test cannot strand the sidecar.
fn wait_for_gate_file(var: &str) {
    let Some(gate) = std::env::var_os(var) else {
        return;
    };
    let gate = PathBuf::from(gate);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !gate.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Test-only gate before the readiness write, so a test can kill the `start`
/// client inside the spawn-to-readiness window without a timing race: the
/// sidecar waits until the file named by `TENDR_TEST_READY_GATE` exists
/// (bounded, so a failed test cannot strand it). Compiled into debug builds
/// only; release sidecars ignore the variable entirely.
fn test_ready_gate() {
    if cfg!(debug_assertions) {
        wait_for_gate_file("TENDR_TEST_READY_GATE");
    }
}

/// Test-only gate between the terminal meta write and the lock release, so a
/// test can observe a session that is terminal but still locked (#91). Waits
/// for the file named by `TENDR_TEST_UNLOCK_GATE` (bounded). Debug builds only.
fn test_unlock_gate() {
    if cfg!(debug_assertions) {
        wait_for_gate_file("TENDR_TEST_UNLOCK_GATE");
    }
}

/// A Write wrapper around Arc<Mutex<Box<dyn Write + Send>>>.
/// Allows multiple owners to write to the same underlying sink.
struct SharedWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("write mutex poisoned"))?
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("write mutex poisoned"))?
            .flush()
    }
}

fn run_inner(session_dir: &Path, ready: &mut Option<ReadyWriter>) -> anyhow::Result<()> {
    // --- Setup: lock, read spec, create meta ---
    let sidecar_identity = Current::self_identity()?;

    let session_name_str = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid session directory"))?;
    let session_name = SessionName::new(session_name_str)?;

    // Path structure: root/<namespace>/<session>/
    let ns_dir = session_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("session dir has no parent (namespace)"))?;
    let namespace_str = ns_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid namespace directory"))?;
    let namespace = Namespace::new(namespace_str)?;

    let root = ns_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("namespace dir has no parent (root)"))?;
    let session_root = SessionRoot::new(root.to_path_buf());
    let session = session::open_raw(&session_root, &namespace, &session_name)?;

    let lock = LockGuard::try_acquire(&session)?;

    // Read launch spec
    let spec_path = session_dir.join("launch_spec.json");
    let spec_json =
        std::fs::read_to_string(&spec_path).context("failed to read launch_spec.json")?;
    let launch_spec: LaunchSpec =
        serde_json::from_str(&spec_json).context("invalid launch_spec.json")?;
    let _ = std::fs::remove_file(&spec_path);

    let run_id = RunId::new();
    let generation = {
        let gen_path = session_dir.join("generation");
        if let Ok(content) = std::fs::read_to_string(&gen_path) {
            let _ = std::fs::remove_file(&gen_path); // consumed
            content
                .trim()
                .parse::<u64>()
                .ok()
                .map(Generation::from_u64)
                .unwrap_or_else(Generation::first)
        } else {
            Generation::first()
        }
    };

    let mut meta = Meta::new_starting(
        session_name.clone(),
        run_id,
        generation,
        launch_spec,
        sidecar_identity,
        EpochTimestamp::now(),
    );

    // The event log records history from the very first state, even though
    // meta only hits disk at Starting for dependency waits (WAL: event first).
    let mut lifecycle =
        LifecycleEvents::new(session_dir, &namespace, &session_name, run_id, generation);
    lifecycle.emit(&mut meta, false);

    // Re-set CLOEXEC on the ready fd before spawning the child.
    // The sidecar inherited this fd with CLOEXEC cleared (so it survived the sidecar's exec),
    // but the child must NOT hold the pipe open -- otherwise the CLI's read_to_string blocks
    // until the child exits, defeating the readiness handshake.
    if let Some(writer) = ready.take() {
        let sealed = Current::seal_ready_fd(writer)
            .map_err(|e| anyhow::anyhow!("failed to seal ready fd: {e}"))?;
        *ready = Some(sealed);
    }

    // Build effective env: user-supplied first, then TENDR_* overlay (authoritative).
    let mut effective_env = meta.launch_spec().env.clone();
    effective_env.insert(
        "TENDR_SESSION".to_owned(),
        meta.session().as_str().to_owned(),
    );
    effective_env.insert("TENDR_NAMESPACE".to_owned(), namespace.as_str().to_owned());
    effective_env.insert("TENDR_RUN_ID".to_owned(), run_id.to_string());
    effective_env.insert("TENDR_GENERATION".to_owned(), generation.to_string());
    effective_env.insert(
        "TENDR_SESSION_DIR".to_owned(),
        session_dir.to_str().unwrap_or("").to_owned(),
    );

    // --- Wait for --after dependencies ---
    let has_deps = !meta.launch_spec().after.is_empty();
    if has_deps {
        // Persist Starting so the session is observable on disk during the scan.
        session::write_meta_atomic(&session, &meta)?;

        // One deadline for the whole wait, fixed before the first scan so that
        // splitting scan-from-poll cannot silently extend the timeout.
        let deadline = meta
            .launch_spec()
            .timeout_s
            .map(|t| std::time::Instant::now() + std::time::Duration::from_secs(t));
        let after_any_exit = meta.launch_spec().after_any_exit;

        // Readiness fires only AFTER the first complete scan, through one funnel.
        // So `start` returning proves every dependency was inspected once (and
        // already-satisfied ones latched), or a terminal condition was persisted
        // and signalled first.
        let outcome = first_dependency_scan(
            &session_root,
            &namespace,
            meta.launch_spec(),
            deadline,
            session_dir,
            &run_id,
        );
        let action = apply_first_scan_outcome(&session, &mut meta, &mut lifecycle, outcome)?;
        signal_readiness(&session, ready, &mut meta);

        match action {
            DepAction::Spawn => {} // every dependency already satisfied — proceed
            DepAction::ReturnTerminal => return Ok(()),
            DepAction::Wait(scanned) => match poll_dependencies(
                scanned,
                &session_root,
                &namespace,
                deadline,
                session_dir,
                &run_id,
                after_any_exit,
            ) {
                DepWaitOutcome::Satisfied => {} // proceed to spawn
                DepWaitOutcome::Failed(msg) => {
                    meta.add_warning(msg);
                    meta.transition_dependency_failed(
                        EpochTimestamp::now(),
                        DepFailReason::Failed,
                    )?;
                    lifecycle.emit(&mut meta, true);
                    session::write_meta_atomic(&session, &meta)?;
                    return Ok(());
                }
                DepWaitOutcome::TimedOut(msg) => {
                    meta.add_warning(msg);
                    meta.transition_dependency_failed(
                        EpochTimestamp::now(),
                        DepFailReason::TimedOut,
                    )?;
                    lifecycle.emit(&mut meta, true);
                    session::write_meta_atomic(&session, &meta)?;
                    return Ok(());
                }
                DepWaitOutcome::Killed(msg) => {
                    meta.add_warning(msg);
                    meta.transition_dependency_failed(
                        EpochTimestamp::now(),
                        DepFailReason::Killed,
                    )?;
                    lifecycle.emit(&mut meta, true);
                    session::write_meta_atomic(&session, &meta)?;
                    return Ok(());
                }
                DepWaitOutcome::KilledForced(msg) => {
                    meta.add_warning(msg);
                    meta.transition_dependency_failed(
                        EpochTimestamp::now(),
                        DepFailReason::KilledForced,
                    )?;
                    lifecycle.emit(&mut meta, true);
                    session::write_meta_atomic(&session, &meta)?;
                    return Ok(());
                }
            },
        }
    }

    // --- Spawn child (with SpawnFailed handling inline) ---
    let spawned = if meta.launch_spec().io_mode == IoMode::Pty {
        Current::spawn_child_pty(
            meta.launch_spec().argv(),
            meta.launch_spec().cwd.as_deref(),
            &effective_env,
        )
    } else {
        Current::spawn_child(
            meta.launch_spec().argv(),
            meta.launch_spec().stdin_mode == crate::model::spec::StdinMode::Pipe,
            meta.launch_spec().cwd.as_deref(),
            &effective_env,
        )
    };
    let child = match spawned {
        Ok(child) => child,
        Err(e) => {
            meta.add_warning(format!("spawn failed: {e}"));
            meta.transition_spawn_failed(EpochTimestamp::now())?;
            lifecycle.emit(&mut meta, true);
            session::write_meta_atomic(&session, &meta)?;
            signal_readiness(&session, ready, &mut meta);
            return Ok(());
        }
    };

    // From here the guard owns the child: no exit below can leave it
    // unsupervised (see supervised_run.rs).
    let run = SupervisedRun::adopt(child, session, lock, meta, lifecycle, ready.take());
    let mut run = run.publish_running()?;
    let how = run.supervise()?;
    run.finish(how)
}

/// Run the `--on-exit` hooks for a terminal run, unlocked and separate from
/// the run lifecycle. Each outcome is a best-effort `callback.finished` fact,
/// and the batch is written under `callbacks/<run_id>.json` outside the
/// session dir, so it survives `--replace`.
fn run_on_exit_hooks(meta: &Meta, session_dir: &Path, lifecycle: &mut LifecycleEvents) {
    let on_exit_callbacks = meta.launch_spec().on_exit.clone();
    if on_exit_callbacks.is_empty() {
        return;
    }
    let RunStatus::Exited { how, .. } = meta.status() else {
        return;
    };
    let exit_reason = exit_reason_env(how);
    let run_id = meta.run_id().to_string();
    let session_name = meta.session().as_str().to_string();
    let namespace = meta
        .launch_spec()
        .namespace
        .as_deref()
        .unwrap_or("default")
        .to_string();
    let generation = meta.generation().to_string();
    let session_dir_str = session_dir.to_str().unwrap_or("").to_string();

    let mut callback_results: Vec<serde_json::Value> = Vec::new();

    for (i, callback_cmd) in on_exit_callbacks.iter().enumerate() {
        let argv = shell_words::split(callback_cmd).unwrap_or_else(|_| vec![callback_cmd.clone()]);
        if argv.is_empty() {
            continue;
        }
        let result = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("TENDR_SESSION", &session_name)
            .env("TENDR_NAMESPACE", &namespace)
            .env("TENDR_RUN_ID", &run_id)
            .env("TENDR_GENERATION", &generation)
            .env("TENDR_EXIT_REASON", &exit_reason)
            .env("TENDR_SESSION_DIR", &session_dir_str)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();

        let record = match result {
            Ok(output) if output.status.success() => {
                serde_json::json!({"index": i, "command": callback_cmd, "status": "ok"})
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                serde_json::json!({
                    "index": i,
                    "command": callback_cmd,
                    "status": "failed",
                    "exit_code": output.status.code(),
                    "stderr": stderr.trim()
                })
            }
            Err(e) => {
                serde_json::json!({
                    "index": i,
                    "command": callback_cmd,
                    "status": "spawn_failed",
                    "error": e.to_string()
                })
            }
        };
        // The durable per-callback fact (plan scope 5): emitted as each
        // callback finishes, same record shape as the batch file.
        lifecycle.append_fact("callback.finished", record.clone());
        callback_results.push(record);
    }

    // Write callback results keyed by run_id, outside the session dir
    // This survives --replace (which removes the session dir)
    let callbacks_dir = session_dir
        .ancestors()
        .find(|p| p.ends_with("sessions"))
        .and_then(|p| p.parent())
        .map(|tendr_root| tendr_root.join("callbacks"));

    if let Some(dir) = callbacks_dir {
        let _ = std::fs::create_dir_all(&dir);
        let record = serde_json::json!({
            "run_id": run_id,
            "session": session_name,
            "namespace": namespace,
            "callbacks": callback_results
        });
        let _ = std::fs::write(dir.join(format!("{run_id}.json")), record.to_string());
    }
}

/// `TENDR_EXIT_REASON` for `--on-exit` hooks. `SidecarFailed` is the bare
/// reason name, so a hook can match it exactly; the failing step is in meta.
/// Every other reason keeps the Debug form hooks have always received.
fn exit_reason_env(how: &ExitReason) -> String {
    match how {
        ExitReason::SidecarFailed { .. } => "SidecarFailed".to_owned(),
        other => format!("{other:?}"),
    }
}

/// Forward data from the stdin transport to the child's stdin pipe.
/// Accepts connections in a loop to support multiple pushes.
/// Exits when: child stdin write fails (child exited) or transport is removed.
fn forward_stdin(
    transport: <Current as Platform>::StdinTransport,
    session_dir: PathBuf,
    mut child_stdin: Box<dyn Write + Send>,
    errors: Arc<Mutex<Vec<String>>>,
) {
    use std::io::Read;
    let mut buf = [0u8; 8192];
    loop {
        // Block until a writer connects (returns None if transport removed)
        let mut reader = match Current::accept_stdin_connection(&transport, &session_dir) {
            Some(r) => r,
            None => return, // transport closed/removed
        };
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break, // writer disconnected
                Ok(n) => n,
                Err(e) => {
                    if let Ok(mut errs) = errors.lock() {
                        errs.push(format!("stdin read failed: {e}"));
                    }
                    return;
                }
            };
            if child_stdin.write_all(&buf[..n]).is_err() {
                if let Ok(mut errs) = errors.lock() {
                    errs.push("stdin forwarding: child stdin closed".to_owned());
                }
                return;
            }
        }
    }
}

/// Outcome of offering the meta snapshot to the `start` client. Deliberately
/// not a `Result`: readiness is a courtesy to the client that asked, never a
/// condition of the run (#71), so it must not be `?`-able.
#[must_use = "an undelivered readiness must be recorded as a session warning"]
enum ReadyDelivery {
    Delivered,
    /// The client has gone (killed mid-handshake, its pane closed).
    ClientGone(io::Error),
    /// Readiness went out earlier (the `--after` path signals after its
    /// first dependency scan).
    AlreadySent,
}

impl ReadyDelivery {
    /// Record a lost delivery in meta. Returns whether meta changed.
    fn record(self, meta: &mut Meta) -> bool {
        match self {
            Self::Delivered | Self::AlreadySent => false,
            Self::ClientGone(e) => {
                meta.add_warning(format!("readiness not delivered: start client gone ({e})"));
                true
            }
        }
    }
}

/// Send the meta snapshot over the readiness channel, consuming the writer.
/// The CLI reads this snapshot directly -- no race with subsequent disk writes.
fn deliver_ready(ready: &mut Option<ReadyWriter>, meta: &Meta) -> ReadyDelivery {
    let Some(writer) = ready.take() else {
        return ReadyDelivery::AlreadySent;
    };
    let message = match serde_json::to_string(meta) {
        Ok(json) => format!("OK:{json}\n"),
        Err(e) => format!("ERROR:meta snapshot not serializable: {e}\n"),
    };
    test_ready_gate();
    if let Err(e) = test_fault(SidecarStep::Readiness.as_str()) {
        drop(writer);
        return ReadyDelivery::ClientGone(e);
    }
    match Current::write_ready_signal(writer, &message) {
        Ok(()) => ReadyDelivery::Delivered,
        Err(e) => ReadyDelivery::ClientGone(e),
    }
}

/// Deliver readiness if it is still owed, and persist a lost delivery as a
/// session warning. Infallible by design: the sidecar then carries on exactly
/// as it would have (supervising, waiting on dependencies, or finishing a
/// terminal path). Meta was already written before every call site, so the
/// rewrite only adds the warning; if the rewrite fails too, the warning stays
/// in memory and the next meta write carries it.
fn signal_readiness(session: &SessionDir, ready: &mut Option<ReadyWriter>, meta: &mut Meta) {
    if deliver_ready(ready, meta).record(meta) {
        let _ = test_fault("ready_rewrite")
            .map_err(|e| e.to_string())
            .and_then(|()| session::write_meta_atomic(session, meta).map_err(|e| e.to_string()));
    }
}

/// Read lines from a stream and write to the shared log file.
/// Returns an error if log writing fails persistently.
fn capture_stream(
    stream: Box<dyn std::io::Read + Send>,
    tag: char,
    log: &Mutex<Box<dyn Write + Send>>,
) -> Result<(), String> {
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // pipe closed
        };
        let formatted = serde_json::to_string(&crate::log::LogLine {
            ts: crate::log::timestamp_secs(),
            tag: tag.to_string(),
            content: serde_json::Value::String(line),
        })
        .expect("JSON serialization cannot fail")
            + "\n";
        let mut f = log.lock().map_err(|e| format!("log mutex poisoned: {e}"))?;
        f.write_all(formatted.as_bytes())
            .map_err(|e| format!("log write failed: {e}"))?;
    }
    Ok(())
}

/// Read raw bytes from a stream, write to log, and tee to the attach sink.
/// Used for PTY sessions where a human may be attached.
fn capture_stream_with_tee(
    mut stream: Box<dyn std::io::Read + Send>,
    tag: char,
    log: &Mutex<Box<dyn Write + Send>>,
    attach_sink: &AttachSink,
) -> Result<(), String> {
    use crate::attach_proto;
    let mut buf = [0u8; 4096];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };

        // Write to log (best-effort chunk-based transcript)
        {
            let mut f = log.lock().map_err(|e| format!("log mutex: {e}"))?;
            let text = String::from_utf8_lossy(&buf[..n]);
            for line in text.lines() {
                let formatted = serde_json::to_string(&crate::log::LogLine {
                    ts: crate::log::timestamp_secs(),
                    tag: tag.to_string(),
                    content: serde_json::Value::String(line.to_owned()),
                })
                .expect("JSON serialization cannot fail")
                    + "\n";
                f.write_all(formatted.as_bytes())
                    .map_err(|e| format!("log write failed: {e}"))?;
            }
        }

        // Tee raw bytes to attached client (if any)
        if let Ok(mut sink_guard) = attach_sink.lock() {
            if let Some(ref mut writer) = *sink_guard {
                if attach_proto::write_msg(writer, attach_proto::MSG_DATA, &buf[..n]).is_err() {
                    *sink_guard = None; // Client disconnected
                }
            }
        }
    }
    Ok(())
}

/// Serve attach clients on an already-bound socket (bound before `Running` is
/// published, so a bind failure is a visible warning, not silence).
#[cfg(unix)]
fn run_attach_listener(
    listener: std::os::unix::net::UnixListener,
    pty_write: Arc<Mutex<Box<dyn Write + Send>>>,
    attach_sink: AttachSink,
    session_dir: &Path,
    resize_fd: Option<std::fs::File>,
    mut facts: LifecycleEvents,
) {
    use crate::attach_proto;

    // Accept connections one at a time
    for stream_result in listener.incoming() {
        let mut read_half = match stream_result {
            Ok(s) => s,
            Err(_) => continue,
        };

        // try_clone for the write half (capture thread tees output here)
        let write_half = match read_half.try_clone() {
            Ok(w) => w,
            Err(_) => continue,
        };

        // Set attach sink -- capture thread starts teeing output
        *attach_sink.lock().unwrap() = Some(Box::new(write_half));

        // WAL-ordered control fact before the meta flip (spec §3.6); the
        // append itself is best-effort, not fsynced.
        // Minimal by design: who owns the PTY's input, nothing else —
        // screen-state semantics are adapter territory (plan boundary note).
        facts.append_fact(
            "pty.control_changed",
            serde_json::json!({"control": "HumanControl", "trigger": "attach"}),
        );
        // Update meta to HumanControl
        set_pty_control_on_disk(session_dir, PtyControl::HumanControl);

        // Read input from human
        loop {
            match attach_proto::read_msg(&mut read_half) {
                Ok((attach_proto::MSG_DATA, payload)) => {
                    if let Ok(mut w) = pty_write.lock() {
                        let _ = w.write_all(&payload);
                        let _ = w.flush();
                    }
                }
                Ok((attach_proto::MSG_RESIZE, payload)) => {
                    if let Some((rows, cols)) = attach_proto::parse_resize(&payload) {
                        if let Some(ref fd) = resize_fd {
                            apply_pty_resize(fd, rows, cols);
                        }
                    }
                }
                Ok((attach_proto::MSG_DETACH, _)) | Err(_) => break,
                _ => {}
            }
        }

        // Clear attach sink -- capture thread stops teeing
        *attach_sink.lock().unwrap() = None;

        facts.append_fact(
            "pty.control_changed",
            serde_json::json!({"control": "AgentControl", "trigger": "detach"}),
        );
        // Update meta to AgentControl
        set_pty_control_on_disk(session_dir, PtyControl::AgentControl);
    }
}

#[cfg(unix)]
fn apply_pty_resize(fd: &std::fs::File, rows: u16, cols: u16) {
    use std::os::unix::io::AsRawFd;
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: fd is a valid PTY master fd. TIOCSWINSZ with a valid winsize
    // pointer is safe on any terminal fd.
    unsafe {
        libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &ws);
    }
}

/// Best-effort: flip the PTY control state in the session's `meta.json` through
/// a typed `Meta` round-trip (read → `set_pty_control` → write). Keeps meta.json
/// a valid typed `Meta` — consistent with how the sidecar serializes meta
/// everywhere else — instead of hand-patching an untyped JSON value with a raw
/// string. Persistence is the same best-effort tmp+rename the attach path has
/// always used (no fsync): a control flip must not block the attach thread, and
/// the durable lifecycle writes go through `write_meta_atomic` elsewhere.
#[cfg(unix)]
fn set_pty_control_on_disk(session_dir: &Path, control: PtyControl) {
    let meta_path = session_dir.join("meta.json");
    let Ok(content) = std::fs::read_to_string(&meta_path) else {
        return;
    };
    let Ok(mut meta) = serde_json::from_str::<Meta>(&content) else {
        return;
    };
    meta.set_pty_control(control);
    let Ok(json) = serde_json::to_string_pretty(&meta) else {
        return;
    };
    let tmp = session_dir.join("meta.json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &meta_path);
    }
}

// Every test here exercises the Unix-only PTY control path.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::model::ids::{EpochTimestamp, Generation, ProcessIdentity, RunId, SessionName};
    use crate::model::pty::PtyControl;
    use crate::model::spec::LaunchSpec;
    use std::num::NonZeroU32;

    /// Write a PTY-enabled meta.json (AgentControl by default) into `dir`.
    fn write_pty_meta(dir: &Path) -> Meta {
        let mut meta = Meta::new_starting(
            SessionName::new("ptytest").unwrap(),
            RunId::new(),
            Generation::first(),
            LaunchSpec::new(vec!["bash".into()]).unwrap(),
            ProcessIdentity {
                pid: NonZeroU32::new(100).unwrap(),
                start_time_ns: 1000,
            },
            EpochTimestamp::now(),
        );
        meta.set_pty(PtyMeta::new());
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        meta
    }

    /// attach -> HumanControl and detach -> AgentControl each leave meta.json a
    /// valid typed Meta carrying the expected pty.control, other fields intact.
    #[test]
    fn set_pty_control_round_trips_as_typed_meta() {
        let dir = tempfile::tempdir().unwrap();
        let original = write_pty_meta(dir.path());
        assert_eq!(original.pty().unwrap().control, PtyControl::AgentControl);

        // attach
        set_pty_control_on_disk(dir.path(), PtyControl::HumanControl);
        let after_attach: Meta =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("meta.json")).unwrap())
                .expect("meta.json still deserializes as typed Meta after attach");
        assert_eq!(
            after_attach.pty().unwrap().control,
            PtyControl::HumanControl
        );
        assert_eq!(
            after_attach.run_id().to_string(),
            original.run_id().to_string(),
            "a control flip must not disturb other meta fields"
        );

        // detach
        set_pty_control_on_disk(dir.path(), PtyControl::AgentControl);
        let after_detach: Meta =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(
            after_detach.pty().unwrap().control,
            PtyControl::AgentControl
        );
    }
}
