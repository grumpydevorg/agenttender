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
use crate::model::pty::{PtyRecording, RecordingState, RecordingStopReason};
use crate::model::spec::{DependencyBinding, IoMode, LaunchSpec};
use crate::model::state::{ExitReason, RunStatus, SidecarStep};
use crate::platform::{Current, Platform};
use crate::recorder::{Recorder, RecorderLimits, RecorderSummary, RecorderThread, StopReason};
use crate::session::{self, LockGuard, SessionDir, SessionRoot};

/// Type alias for the platform's ReadyWriter to avoid verbose turbofish.
type ReadyWriter = <Current as Platform>::ReadyWriter;

/// The human viewer PTY output is teed to, tagged with its controller identity
/// so a stale connection can never clear or replace its successor's viewer.
///
/// Output capture never writes to the viewer's socket. It offers each chunk to
/// the viewer's bounded [`ViewerQueue`], which the viewer's own sender thread
/// drains; a viewer that falls a full budget behind is disconnected instead of
/// stalling capture (and, through capture, the child).
struct AttachViewer {
    #[cfg(unix)]
    holder: crate::model::pty_control::HolderId,
    queue: Arc<ViewerQueue>,
    /// Shut the viewer's socket down (used when it overflows its queue).
    disconnect: Box<dyn Fn() + Send>,
}

/// Queued output bytes one viewer may fall behind before it is disconnected.
const VIEWER_QUEUE_BYTES: usize = 8 << 20;

/// A byte-bounded queue of output chunks for one viewer.
#[derive(Default)]
struct ViewerQueue {
    state: Mutex<ViewerQueueState>,
    ready: std::sync::Condvar,
}

#[derive(Default)]
struct ViewerQueueState {
    chunks: std::collections::VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
}

impl ViewerQueue {
    /// Queue `chunk` without blocking. `false` means the viewer is closed or has
    /// just overflowed its budget; either way it must be dropped.
    fn offer(&self, chunk: &[u8]) -> bool {
        let mut state = lock(&self.state);
        if state.closed {
            return false;
        }
        if state.bytes + chunk.len() > VIEWER_QUEUE_BYTES {
            state.closed = true;
            state.chunks.clear();
            state.bytes = 0;
            drop(state);
            self.ready.notify_all();
            return false;
        }
        state.bytes += chunk.len();
        state.chunks.push_back(chunk.to_vec());
        drop(state);
        self.ready.notify_all();
        true
    }

    /// Wait for the next chunk; `None` once closed.
    #[cfg(unix)]
    fn next(&self) -> Option<Vec<u8>> {
        let mut state = lock(&self.state);
        loop {
            if state.closed {
                return None;
            }
            if let Some(chunk) = state.chunks.pop_front() {
                state.bytes -= chunk.len();
                return Some(chunk);
            }
            state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    #[cfg(unix)]
    fn close(&self) {
        lock(&self.state).closed = true;
        self.ready.notify_all();
    }
}

/// Drain a viewer's queue into its connection until either closes.
#[cfg(unix)]
fn run_viewer_sender(queue: &ViewerQueue, conn: &Mutex<Box<dyn Write + Send>>) {
    use crate::attach_proto;
    while let Some(chunk) = queue.next() {
        if attach_proto::write_msg(&mut *lock(conn), attach_proto::MSG_DATA, &chunk).is_err() {
            queue.close();
            return;
        }
    }
}

/// Shared sink for teeing PTY output to the attached client.
type AttachSink = Arc<Mutex<Option<AttachViewer>>>;

/// Lock a mutex, recovering the data if another thread panicked while holding it.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The sidecar's PTY input authority and live attach connections (Unix only;
/// PTY sessions are unsupported elsewhere, so the type is uninhabited there).
#[cfg(unix)]
struct PtyInput {
    writer: crate::pty_input::InputWriter,
    registry: ConnectionRegistry,
}
#[cfg(not(unix))]
type PtyInput = std::convert::Infallible;

/// Removes a run's attach socket, and its breadcrumb while it still names that
/// socket, when dropped. The run drops it before releasing the session lock, and
/// on every early exit. The socket is this run's own: its name derives from the
/// run identity and binding refused any pre-existing path.
#[cfg(unix)]
struct AttachSocketCleanup {
    socket: PathBuf,
    session_dir: PathBuf,
}

#[cfg(unix)]
impl Drop for AttachSocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        if crate::attach_proto::read_breadcrumb(&self.session_dir).as_ref() == Some(&self.socket) {
            let _ = std::fs::remove_file(self.session_dir.join("a.sock.path"));
        }
    }
}

/// A PTY run's private attach socket, bound and published before the child is
/// spawned (Unix only; the type is uninhabited elsewhere). The lifecycle guard
/// owns it after spawn and drops it, removing the socket, before it releases
/// the session lock.
#[cfg(unix)]
struct AttachEndpoint {
    /// Moved to the listener thread when it starts.
    socket: Option<crate::attach_socket::BoundSocket>,
    /// Held for its `Drop`.
    _cleanup: AttachSocketCleanup,
}
#[cfg(not(unix))]
type AttachEndpoint = std::convert::Infallible;

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
    /// attach listener, the input writer's hooks, the recorder's stop report).
    /// The protocol is multi-writer by design: each writer keeps its own
    /// contiguous `seq` chain (spec §1).
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

/// Forward pushed stdin for a PTY session through the single input writer.
#[cfg(unix)]
fn setup_pty_stdin_forwarding(
    session_dir: &Path,
    input: &PtyInput,
    errors: &Arc<Mutex<Vec<String>>>,
) -> io::Result<()> {
    #[allow(clippy::let_unit_value)]
    let transport = Current::create_stdin_transport(session_dir)?;
    let session_dir = session_dir.to_path_buf();
    let writer = input.writer.clone();
    let errors = Arc::clone(errors);
    std::thread::spawn(move || forward_pty_stdin(transport, &session_dir, &writer, &errors));
    Ok(())
}

#[cfg(not(unix))]
fn setup_pty_stdin_forwarding(
    _session_dir: &Path,
    input: &PtyInput,
    _errors: &Arc<Mutex<Vec<String>>>,
) -> io::Result<()> {
    match *input {}
}

/// Each push connection claims the PTY as an agent for its duration and writes
/// through the input writer, one acknowledged chunk at a time.
///
/// A push that cannot claim (a human holds control) or whose claim is revoked by
/// a takeover has its remaining bytes drained and discarded — never written to
/// the PTY. The FIFO is drained rather than closed because reopening it would
/// hand the still-connected push writer to the next accept. The legacy push CLI
/// therefore cannot observe the rejection; revocation is recorded as a
/// `pty.input_revoked` event and the rejection as a run warning.
#[cfg(unix)]
fn forward_pty_stdin(
    transport: <Current as Platform>::StdinTransport,
    session_dir: &Path,
    writer: &crate::pty_input::InputWriter,
    errors: &Mutex<Vec<String>>,
) {
    use crate::model::pty_control::{ControllerKind, IncompleteReason, InputOutcome};

    let mut buf = [0u8; 8192];
    loop {
        let Some(mut reader) = Current::accept_stdin_connection(&transport, session_dir) else {
            return;
        };
        let handle = match writer.claim(writer.next_holder(), ControllerKind::Agent) {
            Ok(handle) => Some(handle),
            Err(e) => {
                lock(errors).push(format!("push rejected: {e}"));
                None
            }
        };
        let mut authorized = handle;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    lock(errors).push(format!("stdin read failed: {e}"));
                    return;
                }
            };
            let Some(current) = authorized else {
                continue; // rejected or revoked: drain and discard
            };
            match writer.write(current, buf[..n].to_vec()) {
                Some(InputOutcome::Accepted { .. }) => {}
                Some(InputOutcome::Incomplete {
                    reason: IncompleteReason::NotAuthorized(_),
                    ..
                }) => authorized = None,
                Some(InputOutcome::Incomplete { .. }) | None => {
                    lock(errors).push("stdin forwarding: child stdin closed".to_owned());
                    return;
                }
            }
        }
        if let Some(handle) = handle {
            // Release only after this push's input is done, so the next push can
            // claim; a revoked handle releases nothing.
            let _ = writer.end_of_input_and_wait(handle);
        }
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
    // A PTY session's private attach socket is bound and published before the
    // child exists, so a PTY session never runs without its listener. A bind
    // failure is a spawn failure: there is no child yet. Dropping the endpoint
    // removes the socket and its breadcrumb: here if the spawn fails, otherwise
    // in the lifecycle guard before it releases the session lock.
    #[cfg(unix)]
    let attach = if meta.launch_spec().io_mode == IoMode::Pty {
        match crate::attach_socket::bind_for_session(session_dir, run_id) {
            Ok(bound) => Some(AttachEndpoint {
                _cleanup: AttachSocketCleanup {
                    socket: bound.path.clone(),
                    session_dir: session_dir.to_path_buf(),
                },
                socket: Some(bound),
            }),
            Err(e) => {
                meta.add_warning(format!("attach socket unavailable: {e}"));
                meta.transition_spawn_failed(EpochTimestamp::now())?;
                lifecycle.emit(&mut meta, true);
                session::write_meta_atomic(&session, &meta)?;
                signal_readiness(&session, ready, &mut meta);
                return Ok(());
            }
        }
    } else {
        None
    };
    #[cfg(not(unix))]
    let attach: Option<AttachEndpoint> = None;

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
    let run = SupervisedRun::adopt(child, session, lock, meta, lifecycle, ready.take(), attach);
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

/// Read raw bytes from a stream, record them, write them to the log, and tee
/// them to the attach sink. Used for PTY sessions where a human may be attached.
fn capture_stream_with_tee(
    mut stream: Box<dyn std::io::Read + Send>,
    tag: char,
    log: &Mutex<Box<dyn Write + Send>>,
    attach_sink: &AttachSink,
    recorder: Option<&Recorder>,
) -> Result<(), String> {
    let mut buf = [0u8; 4096];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };

        // Record first: sequencing never waits for storage.
        if let Some(recorder) = recorder {
            recorder.output(&buf[..n]);
        }

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

        // Offer raw bytes to the attached viewer's bounded queue; never block.
        let mut sink_guard = lock(attach_sink);
        if let Some(viewer) = sink_guard.as_ref() {
            if !viewer.queue.offer(&buf[..n]) {
                (viewer.disconnect)();
                *sink_guard = None; // closed, or fell a full budget behind
            }
        }
    }
    Ok(())
}

/// The PTY input the single writer drives: the master's write half, a dup used
/// for window-size changes, and the recorder that records applied sizes.
#[cfg(unix)]
struct UnixPtyInput {
    writer: crate::platform::unix::PtyWriter,
    resize: Option<std::fs::File>,
    recorder: Option<Recorder>,
}

#[cfg(unix)]
impl crate::model::pty_control::PtyInputSink for UnixPtyInput {
    fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer.try_write(bytes)
    }
}

#[cfg(unix)]
impl crate::pty_input::WritablePty for UnixPtyInput {
    fn wait_writable(&self, timeout: std::time::Duration) -> io::Result<bool> {
        self.writer.wait_writable(timeout)
    }

    /// A size with a zero dimension is ignored: it cannot be recorded, and no
    /// terminal program can draw into it.
    fn resize(&self, rows: u16, cols: u16) {
        let (Some(fd), Some(geometry)) =
            (&self.resize, crate::recording::Geometry::new(rows, cols))
        else {
            return;
        };
        let apply = || apply_pty_resize(fd, rows, cols);
        let _ = match &self.recorder {
            Some(recorder) => {
                recorder.resize_applied(geometry, crate::recording::ResizeCause::User, apply)
            }
            None => apply(),
        };
    }
}

/// A live attach-socket connection: a descriptor used only to shut the socket
/// down, and how to retire it when a takeover supersedes it.
#[cfg(unix)]
struct Registered {
    control: std::os::unix::net::UnixStream,
    retirement: Retirement,
}

/// How a superseded connection is retired.
#[cfg(unix)]
enum Retirement {
    /// An attach client: told with `MSG_RETIRED` through its framed writer
    /// (shared with its output sender), then shut down.
    Viewer {
        conn: Arc<Mutex<Box<dyn Write + Send>>>,
    },
    /// A push: flagged revoked and its read side shut down, which wakes a handler
    /// waiting for the next frame; the handler then reports the outcome itself.
    Push { revoked: Arc<AtomicBool> },
}

/// Live attach connections by controller identity, so a takeover can retire
/// exactly the superseded connection.
#[cfg(unix)]
#[derive(Clone, Default)]
struct ConnectionRegistry(Arc<Mutex<RegistryState>>);

#[cfg(unix)]
#[derive(Default)]
struct RegistryState {
    live: std::collections::HashMap<crate::model::pty_control::HolderId, Registered>,
    /// Set when the run ends; nothing registers after that.
    closed: bool,
}

#[cfg(unix)]
impl ConnectionRegistry {
    /// Register a connection. Returns false, dropping `entry`, once the run has
    /// ended: the caller must reject the connection.
    #[must_use]
    fn insert(&self, holder: crate::model::pty_control::HolderId, entry: Registered) -> bool {
        let mut state = lock(&self.0);
        if state.closed {
            return false;
        }
        state.live.insert(holder, entry);
        true
    }

    fn remove(&self, holder: crate::model::pty_control::HolderId) -> Option<Registered> {
        lock(&self.0).live.remove(&holder)
    }

    fn contains(&self, holder: crate::model::pty_control::HolderId) -> bool {
        lock(&self.0).live.contains_key(&holder)
    }

    /// Refuse further registrations and shut every live connection down, so
    /// each client sees end of stream and each handler thread exits. The same
    /// lock as `insert`, so no connection can slip in after the sweep.
    fn close(&self) {
        let entries: Vec<Registered> = {
            let mut state = lock(&self.0);
            state.closed = true;
            state.live.drain().map(|(_, entry)| entry).collect()
        };
        for entry in entries {
            if let Retirement::Push { revoked } = &entry.retirement {
                revoked.store(true, Ordering::SeqCst);
            }
            let _ = entry.control.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Ends a PTY session's attach side when its run ends, before the terminal
/// record is written: the control hooks stop touching disk (after
/// `start --replace` the session paths belong to the next run), and every
/// connection is shut down (PR #68 review, finding 1).
#[cfg(unix)]
struct PtyTeardown {
    ended: Arc<AtomicBool>,
    registry: ConnectionRegistry,
}

#[cfg(unix)]
impl PtyTeardown {
    fn run(&self) {
        self.ended.store(true, Ordering::SeqCst);
        self.registry.close();
    }
}
#[cfg(not(unix))]
type PtyTeardown = std::convert::Infallible;

/// Maximum simultaneous attach connections, including ones awaiting a hello.
#[cfg(unix)]
const MAX_ATTACH_CONNECTIONS: usize = 8;

/// Ownership side effects, run on the input writer thread.
#[cfg(unix)]
struct SidecarControlHooks {
    session_dir: PathBuf,
    facts: LifecycleEvents,
    registry: ConnectionRegistry,
    attach_sink: AttachSink,
    /// The live owner, read by the guard's whole-meta writes.
    control: Arc<Mutex<PtyControl>>,
    /// Set when the run ends: from then on the hooks record nothing.
    ended: Arc<AtomicBool>,
}

#[cfg(unix)]
impl SidecarControlHooks {
    /// Publish the new owner to the guard before patching `meta.json`: a guard
    /// write that already read the old owner holds [`META_WRITE`], so this
    /// patch lands after it and the file ends on the new owner either way.
    fn set_control(&self, control: PtyControl) {
        *lock(&self.control) = control.clone();
        set_pty_control_on_disk(&self.session_dir, control);
    }
}

#[cfg(unix)]
impl crate::pty_input::ControlHooks for SidecarControlHooks {
    fn human_control(&mut self, trigger: &'static str) {
        if self.ended.load(Ordering::SeqCst) {
            return;
        }
        // WAL-ordered control fact before the meta flip (spec §3.6); the
        // append itself is best-effort, not fsynced. Minimal by design: who
        // owns the PTY's input, nothing else.
        self.facts.append_fact(
            "pty.control_changed",
            serde_json::json!({"control": "HumanControl", "trigger": trigger}),
        );
        self.set_control(PtyControl::HumanControl);
    }

    fn human_released(&mut self) {
        if self.ended.load(Ordering::SeqCst) {
            return;
        }
        self.facts.append_fact(
            "pty.control_changed",
            serde_json::json!({"control": "AgentControl", "trigger": "detach"}),
        );
        self.set_control(PtyControl::AgentControl);
    }

    fn retire(
        &mut self,
        holder: crate::model::pty_control::HolderId,
        kind: crate::model::pty_control::ControllerKind,
        epoch: crate::model::pty_control::ControllerEpoch,
    ) {
        // Forget the connection now, on the writer thread, before the successor
        // is granted control: from here on it can never install a viewer. An
        // unregistered holder (a FIFO exec forwarder) learns of revocation from
        // its write outcomes.
        let _ = kind;
        let Some(entry) = self.registry.remove(holder) else {
            return;
        };
        match entry.retirement {
            Retirement::Push { revoked } => {
                // Nonblocking: interrupt a handler idling on its next frame. A
                // handler waiting on a PTY write gets a NotAuthorized outcome.
                revoked.store(true, Ordering::SeqCst);
                let _ = entry.control.shutdown(std::net::Shutdown::Read);
            }
            Retirement::Viewer { conn } => {
                // Notifying and shutting down may wait on a stalled connection;
                // never block the writer with that.
                let sink = Arc::clone(&self.attach_sink);
                let control = entry.control;
                std::thread::spawn(move || {
                    retire_connection(holder, epoch, &conn, &control, &sink)
                });
            }
        }
    }

    fn input_revoked(
        &mut self,
        kind: crate::model::pty_control::ControllerKind,
        accepted: usize,
        total: usize,
    ) {
        if self.ended.load(Ordering::SeqCst) {
            return;
        }
        self.facts.append_fact(
            "pty.input_revoked",
            serde_json::json!({"kind": format!("{kind:?}"), "accepted": accepted, "total": total}),
        );
    }
}

/// Finish retiring a connection the takeover hook has already removed from the
/// registry: tell it best-effort, shut the socket down (which also unblocks any
/// output write stuck on it), and drop its viewer if it had installed one.
#[cfg(unix)]
fn retire_connection(
    holder: crate::model::pty_control::HolderId,
    epoch: crate::model::pty_control::ControllerEpoch,
    conn: &Mutex<Box<dyn Write + Send>>,
    control: &std::os::unix::net::UnixStream,
    sink: &AttachSink,
) {
    use crate::attach_proto;

    let _ = control.set_write_timeout(Some(std::time::Duration::from_millis(200)));
    for _ in 0..20 {
        if let Ok(mut conn) = conn.try_lock() {
            let _ = attach_proto::write_msg(
                &mut *conn,
                attach_proto::MSG_RETIRED,
                &epoch.get().to_be_bytes(),
            );
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = control.shutdown(std::net::Shutdown::Both);
    let mut viewer = lock(sink);
    if viewer.as_ref().is_some_and(|v| v.holder == holder) {
        if let Some(v) = viewer.take() {
            v.queue.close();
        }
    }
}

/// One acknowledged agent push: claim control if nobody holds it, write each
/// received frame before reading the next (so the client feels the PTY's
/// backpressure), and report exactly what was written.
///
/// The claim is released only after all input is written. Neither of the
/// handler's two waits can hold control hostage:
///
/// - **waiting for the next frame:** a takeover retires the registered push,
///   which flags it revoked and shuts its read side down; the handler wakes and
///   reports `revoked`;
/// - **waiting for a PTY write:** a takeover makes the write outcome
///   `NotAuthorized`; a client that vanishes is noticed by polling for hang-up,
///   and its input is cancelled.
///
/// A push that stops early reports how far it got. A vanished client gets no
/// report; its connection is simply gone.
#[cfg(unix)]
fn handle_push_connection(
    mut stream: std::os::unix::net::UnixStream,
    writer: &crate::pty_input::InputWriter,
    registry: &ConnectionRegistry,
) {
    use crate::attach_proto::{self, INPUT_CLOSED, INPUT_REVOKED, INPUT_WRITTEN};
    use crate::model::pty_control::{ControllerKind, IncompleteReason, InputOutcome};

    let holder = writer.next_holder();
    let revoked = Arc::new(AtomicBool::new(false));
    let Ok(control) = stream.try_clone() else {
        return;
    };
    // Register before claiming, so a takeover right after the grant retires it.
    let registered = registry.insert(
        holder,
        Registered {
            control,
            retirement: Retirement::Push {
                revoked: Arc::clone(&revoked),
            },
        },
    );
    if !registered {
        return reject_connection(&mut stream, "session ended");
    }
    let handle = match writer.claim(holder, ControllerKind::Agent) {
        Ok(handle) => handle,
        Err(e) => {
            registry.remove(holder);
            return reject_connection(&mut stream, &e.to_string());
        }
    };
    let gone = |stream: &std::os::unix::net::UnixStream| {
        registry.remove(holder);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        writer.disconnect(handle);
    };
    if attach_proto::write_msg(
        &mut stream,
        attach_proto::MSG_ACCEPTED,
        &handle.epoch().get().to_be_bytes(),
    )
    .is_err()
    {
        return gone(&stream);
    }

    let (mut accepted, mut received) = (0u64, 0u64);
    let status = loop {
        match attach_proto::read_msg(&mut stream) {
            Ok((attach_proto::MSG_DATA, payload)) => {
                received += payload.len() as u64;
                let Some(outcome) = writer.submit(handle, payload) else {
                    continue; // an empty frame writes nothing
                };
                match await_push_write(&outcome, &stream, &revoked, writer, handle) {
                    PushWrite::Done(InputOutcome::Accepted { bytes, .. }) => {
                        accepted += bytes as u64;
                    }
                    PushWrite::Done(InputOutcome::Incomplete {
                        accepted: partial,
                        reason,
                        ..
                    }) => {
                        accepted += partial as u64;
                        break if matches!(reason, IncompleteReason::NotAuthorized(_)) {
                            INPUT_REVOKED
                        } else {
                            INPUT_CLOSED
                        };
                    }
                    PushWrite::WriterGone => break INPUT_CLOSED,
                    PushWrite::ClientGone => return gone(&stream),
                }
            }
            Ok((attach_proto::MSG_DETACH, _)) => {
                // Every earlier frame was written before the next was read, so
                // nothing is queued: the release happens at once.
                break if writer.end_of_input_and_wait(handle) {
                    INPUT_WRITTEN
                } else {
                    INPUT_REVOKED
                };
            }
            Ok(_) => {}
            Err(_) if revoked.load(Ordering::SeqCst) => break INPUT_REVOKED,
            // The client vanished or broke the protocol: cancel its input.
            Err(_) => return gone(&stream),
        }
    };
    registry.remove(holder);
    if status != INPUT_WRITTEN {
        writer.disconnect(handle); // release if still held; nothing else is queued
    }
    let _ = attach_proto::write_msg(
        &mut stream,
        attach_proto::MSG_INPUT_DONE,
        &attach_proto::input_done_payload(status, accepted, received),
    );
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// How waiting for one push write ended.
#[cfg(unix)]
enum PushWrite {
    Done(crate::model::pty_control::InputOutcome),
    WriterGone,
    ClientGone,
}

/// Wait for a push write's outcome, watching for the client to vanish. A
/// vanished client's input is cancelled before returning, so the wait cannot
/// outlast the PTY accepting nothing.
#[cfg(unix)]
fn await_push_write(
    outcome: &std::sync::mpsc::Receiver<crate::model::pty_control::InputOutcome>,
    stream: &std::os::unix::net::UnixStream,
    revoked: &AtomicBool,
    writer: &crate::pty_input::InputWriter,
    handle: crate::model::pty_control::ControllerHandle,
) -> PushWrite {
    use std::sync::mpsc::RecvTimeoutError;

    loop {
        match outcome.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(done) => return PushWrite::Done(done),
            Err(RecvTimeoutError::Disconnected) => return PushWrite::WriterGone,
            // A retired push shut its own read side down; its write outcome
            // (NotAuthorized) is what ends the wait, not a hang-up.
            Err(RecvTimeoutError::Timeout)
                if !revoked.load(Ordering::SeqCst)
                    && crate::attach_socket::peer_hung_up(stream) =>
            {
                writer.disconnect(handle); // cancels this write
                let _ = outcome.recv();
                return PushWrite::ClientGone;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Install `viewer` unless its connection has been retired.
///
/// The registry check and the install happen under the sink lock. Combined
/// with retirement removing the holder from the registry synchronously (on the
/// writer thread, before the successor is even granted control), a retired
/// connection can never install its viewer over its successor's.
#[cfg(unix)]
fn install_viewer(registry: &ConnectionRegistry, sink: &AttachSink, viewer: AttachViewer) -> bool {
    let mut current = lock(sink);
    if registry.contains(viewer.holder) {
        *current = Some(viewer);
        true
    } else {
        false
    }
}

#[cfg(unix)]
fn run_attach_listener(
    listener: &std::os::unix::net::UnixListener,
    writer: &crate::pty_input::InputWriter,
    registry: &ConnectionRegistry,
    attach_sink: &AttachSink,
) {
    use std::sync::atomic::AtomicUsize;

    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        // Only the run owner's own processes may attach. The socket directory
        // is already owner-only; this checks the connecting process itself.
        if crate::attach_socket::verify_peer(&stream).is_err() {
            reject_connection(&mut stream, "peer identity rejected");
            continue;
        }
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_ATTACH_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            reject_connection(&mut stream, "too many attach connections");
            continue;
        }
        let (writer, registry, sink, active) = (
            writer.clone(),
            registry.clone(),
            Arc::clone(attach_sink),
            Arc::clone(&active),
        );
        std::thread::spawn(move || {
            handle_attach_connection(stream, &writer, &registry, &sink);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

#[cfg(unix)]
fn reject_connection(stream: &mut std::os::unix::net::UnixStream, reason: &str) {
    use crate::attach_proto;
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(200)));
    let _ = attach_proto::write_msg(stream, attach_proto::MSG_REJECTED, reason.as_bytes());
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// One attach connection: v1 hello, claim or takeover through the input writer,
/// then relay input and resizes until detach, disconnect, or retirement.
#[cfg(unix)]
fn handle_attach_connection(
    mut stream: std::os::unix::net::UnixStream,
    writer: &crate::pty_input::InputWriter,
    registry: &ConnectionRegistry,
    attach_sink: &AttachSink,
) {
    use crate::attach_proto::{
        self, MODE_ATTACH, MODE_PUSH, MODE_TAKEOVER, MSG_HELLO, PROTOCOL_VERSION,
    };
    use crate::model::pty_control::ControllerKind;

    // One overall deadline for the whole hello: a client trickling bytes cannot
    // hold a connection slot open by keeping each individual read short.
    let hello = attach_proto::read_msg(&mut crate::attach_socket::DeadlineReader {
        stream: &stream,
        deadline: std::time::Instant::now() + attach_proto::HELLO_TIMEOUT,
    });
    let mode = match hello {
        Ok((MSG_HELLO, p))
            if p.len() == 2
                && p[0] == PROTOCOL_VERSION
                && matches!(p[1], MODE_ATTACH | MODE_TAKEOVER | MODE_PUSH) =>
        {
            p[1]
        }
        Ok((MSG_HELLO, _)) => {
            return reject_connection(&mut stream, "unsupported attach protocol version or mode");
        }
        _ => return reject_connection(&mut stream, "attach requires a protocol hello"),
    };
    let _ = stream.set_read_timeout(None);
    if mode == MODE_PUSH {
        return handle_push_connection(stream, writer, registry);
    }

    let (Ok(conn_stream), Ok(control)) = (stream.try_clone(), stream.try_clone()) else {
        return;
    };
    let conn: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(Box::new(conn_stream)));
    let holder = writer.next_holder();
    // Register before asking for control, so a takeover that lands immediately
    // after the grant can still find and retire this connection.
    let registered = registry.insert(
        holder,
        Registered {
            control,
            retirement: Retirement::Viewer {
                conn: Arc::clone(&conn),
            },
        },
    );
    if !registered {
        return reject_connection(&mut stream, "session ended");
    }
    let granted = if mode == MODE_TAKEOVER {
        writer.takeover(holder).map(|t| t.handle)
    } else {
        writer.claim(holder, ControllerKind::Human)
    };
    let handle = match granted {
        Ok(handle) => handle,
        Err(e) => {
            registry.remove(holder);
            return reject_connection(&mut stream, &e.to_string());
        }
    };

    // Reply before installing the viewer, so tee output never precedes it.
    let accepted = attach_proto::write_msg(
        &mut *lock(&conn),
        attach_proto::MSG_ACCEPTED,
        &handle.epoch().get().to_be_bytes(),
    );
    let queue = Arc::new(ViewerQueue::default());
    if accepted.is_ok() {
        let sender = {
            let (queue, conn) = (Arc::clone(&queue), Arc::clone(&conn));
            std::thread::spawn(move || run_viewer_sender(&queue, &conn))
        };
        if let Ok(shutdown) = stream.try_clone() {
            // Refused if this connection was retired in the meantime.
            install_viewer(
                registry,
                attach_sink,
                AttachViewer {
                    holder,
                    queue: Arc::clone(&queue),
                    disconnect: Box::new(move || {
                        let _ = shutdown.shutdown(std::net::Shutdown::Both);
                    }),
                },
            );
        }
        drop(sender); // detached: exits when the queue or connection closes

        loop {
            match attach_proto::read_msg(&mut stream) {
                Ok((attach_proto::MSG_DATA, payload)) => {
                    // A full input queue must not hide a client that has left.
                    let queued = writer.write_nowait_unless(handle, payload, || {
                        crate::attach_socket::peer_hung_up(&stream)
                    });
                    if !queued {
                        break;
                    }
                }
                Ok((attach_proto::MSG_RESIZE, payload)) => {
                    if let Some((rows, cols)) = attach_proto::parse_resize(&payload) {
                        writer.resize(handle, rows, cols);
                    }
                }
                Ok((attach_proto::MSG_DETACH, _)) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }

    // Detach, disconnect, retirement, or a protocol violation such as an
    // oversized frame: close the connection, release ahead of queued input,
    // and cancel what this client still has queued.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    writer.disconnect(handle);
    registry.remove(holder);
    let mut viewer = lock(attach_sink);
    if viewer.as_ref().is_some_and(|v| v.holder == holder) {
        *viewer = None;
    }
    drop(viewer);
    queue.close();
}

#[cfg(unix)]
fn apply_pty_resize(fd: &std::fs::File, rows: u16, cols: u16) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: fd is a valid PTY master fd. TIOCSWINSZ with a valid winsize
    // pointer is safe on any terminal fd.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &ws) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Serializes the sidecar's `meta.json` writes once helper threads can patch it:
/// the patches below, and the lifecycle guard's writes, which fold in the
/// recording state they read under this lock.
static META_WRITE: Mutex<()> = Mutex::new(());

/// Best-effort: flip the PTY control state in the session's `meta.json`.
#[cfg(unix)]
fn set_pty_control_on_disk(session_dir: &Path, control: PtyControl) {
    patch_meta_on_disk(session_dir, |meta| meta.set_pty_control(control));
}

/// Best-effort: patch the session's `meta.json` through a typed `Meta`
/// round-trip (read → `patch` → write). Keeps meta.json a valid typed `Meta` —
/// consistent with how the sidecar serializes meta everywhere else — instead of
/// hand-patching an untyped JSON value. Persistence is the same best-effort
/// tmp+rename the attach path has always used (no fsync): a patch must not block
/// the thread making it for long, and the durable lifecycle writes go through
/// `write_meta_atomic` elsewhere.
fn patch_meta_on_disk(session_dir: &Path, patch: impl FnOnce(&mut Meta)) {
    let _serialized = lock(&META_WRITE);
    let meta_path = session_dir.join("meta.json");
    let Ok(content) = std::fs::read_to_string(&meta_path) else {
        return;
    };
    let Ok(mut meta) = serde_json::from_str::<Meta>(&content) else {
        return;
    };
    patch(&mut meta);
    let Ok(json) = serde_json::to_string_pretty(&meta) else {
        return;
    };
    let tmp = session_dir.join("meta.json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &meta_path);
    }
}

/// A PTY run's recording: the recorder and where its segments live.
struct PtyRecordingRun {
    thread: RecorderThread,
    /// Relative to the session directory.
    dir: String,
}

impl PtyRecordingRun {
    fn meta(&self, state: RecordingState) -> PtyRecording {
        PtyRecording {
            dir: self.dir.clone(),
            input_recorded: false,
            state,
        }
    }
}

/// Start recording a PTY run: output and applied geometry, not input. The child
/// starts at the platform's initial PTY size, under its `TERM` (recorded empty if
/// unset or not recordable). A stop is appended as a `recording.stopped` event,
/// then written to `meta.json`.
fn start_pty_recording(
    session_dir: &Path,
    run_id: RunId,
    child_env: &std::collections::BTreeMap<String, String>,
    mut facts: LifecycleEvents,
) -> PtyRecordingRun {
    use crate::recording::{Geometry, SegmentHeader, Sequence, TermName};

    let dir = format!("recording/{run_id}");
    let term = child_env
        .get("TERM")
        .cloned()
        .or_else(|| std::env::var("TERM").ok())
        .and_then(|term| TermName::new(term).ok())
        .unwrap_or_else(|| TermName::new("").expect("an empty TERM is recordable"));
    let origin_unix_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    let header = SegmentHeader {
        run_id,
        segment_index: 0,
        first_sequence: Sequence::FIRST,
        origin_unix_ns,
        geometry: Geometry::new(
            crate::platform::INITIAL_PTY_ROWS,
            crate::platform::INITIAL_PTY_COLS,
        )
        .expect("the initial PTY size is nonzero"),
        input_recorded: false,
        term,
    };

    let store = crate::recorder::DirectoryStore::new(session_dir.join(&dir));
    let report_dir = session_dir.to_path_buf();
    let thread = RecorderThread::start(header, store, recorder_limits(), move |stopped| {
        let mut data = serde_json::json!({
            "last_recorded_sequence": stopped.last_recorded.map(Sequence::get),
            "reason": stopped.reason.as_str(),
        });
        if let StopReason::WriteFailed(kind) = stopped.reason {
            data["error"] = serde_json::Value::String(format!("{kind:?}"));
        }
        facts.append_fact("recording.stopped", data);
        let state = stopped_state(stopped, None);
        patch_meta_on_disk(&report_dir, |meta| {
            if let Some(mut recording) = meta.pty().and_then(|p| p.recording.clone()) {
                recording.state = state;
                meta.set_pty_recording(recording);
            }
        });
    });
    PtyRecordingRun { thread, dir }
}

/// Recording limits. Debug builds accept `TENDR_TEST_RECORDING_MAX_BYTES` so
/// tests can reach the size limit; release sidecars ignore it.
fn recorder_limits() -> RecorderLimits {
    let mut limits = RecorderLimits::default();
    if cfg!(debug_assertions) {
        if let Some(max) = std::env::var("TENDR_TEST_RECORDING_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            limits.max_bytes = max;
        }
    }
    limits
}

fn stopped_state(
    stopped: crate::recorder::Stopped,
    last_synced: Option<crate::recording::Sequence>,
) -> RecordingState {
    let (reason, error) = match stopped.reason {
        StopReason::SizeLimit => (RecordingStopReason::SizeLimit, None),
        StopReason::WriteFailed(kind) => {
            (RecordingStopReason::WriteFailed, Some(format!("{kind:?}")))
        }
        StopReason::BacklogFull => (RecordingStopReason::BacklogFull, None),
        StopReason::Stalled => (RecordingStopReason::Stalled, None),
    };
    RecordingState::Stopped {
        last_recorded_sequence: stopped.last_recorded.map(crate::recording::Sequence::get),
        last_synced_sequence: last_synced.map(crate::recording::Sequence::get),
        reason,
        error,
    }
}

/// The recording's final state, and a run warning if it stopped early.
fn finished_recording(summary: RecorderSummary) -> (RecordingState, Option<String>) {
    let Some(stopped) = summary.stopped else {
        return (
            RecordingState::Complete {
                last_recorded_sequence: summary.last_recorded.map(crate::recording::Sequence::get),
                last_synced_sequence: summary.last_synced.map(crate::recording::Sequence::get),
            },
            None,
        );
    };
    let at = stopped.last_recorded.map_or_else(
        || "before its first record".to_owned(),
        |s| format!("at sequence {}", s.get()),
    );
    let cause = match stopped.reason {
        StopReason::WriteFailed(kind) => format!("write_failed: {kind:?}"),
        reason => reason.as_str().to_owned(),
    };
    (
        stopped_state(stopped, summary.last_synced),
        Some(format!(
            "recording stopped {at} ({cause}); later output was not recorded"
        )),
    )
}

// Every test here exercises the Unix-only PTY control path.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::model::ids::{EpochTimestamp, Generation, ProcessIdentity, RunId, SessionName};
    use crate::model::pty::PtyControl;
    use crate::model::spec::LaunchSpec;
    use std::num::NonZeroU32;

    #[test]
    fn takeover_retirement_refuses_the_old_viewer_before_the_hook_returns() {
        use crate::model::pty_control::{ControllerEpoch, ControllerKind, HolderId};
        use crate::pty_input::ControlHooks;
        use std::os::unix::net::UnixStream;

        let dir = tempfile::tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let sink: AttachSink = Arc::new(Mutex::new(None));
        let (old_conn, _peer) = UnixStream::pair().unwrap();
        let old = HolderId::new(1);
        assert!(registry.insert(
            old,
            Registered {
                retirement: Retirement::Viewer {
                    conn: Arc::new(Mutex::new(Box::new(old_conn.try_clone().unwrap()))),
                },
                control: old_conn,
            },
        ));
        let mut hooks = SidecarControlHooks {
            session_dir: dir.path().to_path_buf(),
            facts: LifecycleEvents::new(
                dir.path(),
                &Namespace::new("default").unwrap(),
                &SessionName::new("retire").unwrap(),
                RunId::new(),
                Generation::first(),
            ),
            registry: registry.clone(),
            attach_sink: Arc::clone(&sink),
            control: Arc::new(Mutex::new(PtyControl::AgentControl)),
            ended: Arc::new(AtomicBool::new(false)),
        };

        hooks.retire(old, ControllerKind::Human, ControllerEpoch::new(2));

        // No waiting: the old connection may try to install at any instant after
        // the takeover is decided, including before any notification thread runs.
        let installed = install_viewer(
            &registry,
            &sink,
            AttachViewer {
                holder: old,
                queue: Arc::new(ViewerQueue::default()),
                disconnect: Box::new(|| {}),
            },
        );
        assert!(!installed, "a retired connection installed its viewer");
        assert!(lock(&sink).is_none());
    }

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
