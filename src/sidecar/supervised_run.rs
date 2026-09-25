//! The lifecycle guard: from the moment the child is spawned until a terminal
//! state is recorded, a [`SupervisedRun`] owns it.
//!
//! The invariant: after spawn, either the sidecar is alive and owns the child,
//! or meta is terminal and the child is dead, or the sidecar died by a true
//! crash (SIGKILL, OOM, abort) and reconciliation closes the gap. The guard
//! makes the second case the only way out of the first:
//!
//! - [`SupervisedRun<Spawned>::publish_running`] is the only way to `Running`,
//!   so the resources `Running` promises (stdin transport, attach socket) are
//!   in place before it is published.
//! - [`SupervisedRun<Running>::finish`] is the only normal exit.
//! - Any other exit (an error, an early return, a panic unwind) kills the child
//!   through the platform kill path and records
//!   `Exited { reason: SidecarFailed { step } }`, then runs the hooks.
//!
//! Typestate stops at the process boundary: persisted meta stays
//! runtime-validated (`transition.rs`), because a type parameter does not
//! survive serialization.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::marker::PhantomData;
use std::num::NonZeroI32;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    AttachSink, LifecycleEvents, ReadyWriter, SharedWriter, capture_stream,
    capture_stream_with_tee, collect_warnings, run_on_exit_hooks, setup_kill_watcher,
    setup_stdin_forwarding, setup_timeout, signal_readiness, test_abort_point, test_fault,
};
use crate::model::ids::{EpochTimestamp, ProcessIdentity};
use crate::model::meta::Meta;
use crate::model::pty::PtyMeta;
use crate::model::spec::{IoMode, StdinMode};
use crate::model::state::{ExitReason, SidecarStep};
use crate::platform::{Current, Platform, ProcessStatus};
use crate::session::{self, LockGuard, SessionDir};

type SupervisedChild = <Current as Platform>::SupervisedChild;
type ChildKillHandle = <Current as Platform>::ChildKillHandle;

/// The child is spawned; `Running` is not yet published.
pub(super) struct Spawned;
/// `Running` is published; supervision is under way.
pub(super) struct Running;

/// Owner of a spawned child. See the module docs for the contract.
#[must_use = "dropping a SupervisedRun kills the child and records SidecarFailed"]
pub(super) struct SupervisedRun<Phase> {
    core: RunCore,
    _phase: PhantomData<Phase>,
}

/// The guard has already killed the child and recorded the failure; the
/// sidecar only has to exit.
#[derive(Debug)]
pub(super) struct SidecarFailure {
    step: SidecarStep,
}

impl std::fmt::Display for SidecarFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sidecar failed at {}; the child was stopped and the failure recorded",
            self.step
        )
    }
}

impl std::error::Error for SidecarFailure {}

/// Everything the guard owns. `Drop` is here, not on [`SupervisedRun`], so a
/// phase change can move the core into a new wrapper without running it.
struct RunCore {
    child: SupervisedChild,
    kill_handle: ChildKillHandle,
    identity: ProcessIdentity,
    meta: Meta,
    lifecycle: LifecycleEvents,
    session: SessionDir,
    /// The `start` client's readiness channel while it is still unsent.
    ready: Option<ReadyWriter>,
    stdin_errors: Arc<Mutex<Vec<String>>>,
    /// Tee for PTY output to an attached client; `None` for pipe sessions.
    attach_sink: Option<AttachSink>,
    /// Stops the timeout and kill-request watchers once the run is ending.
    watch_cancel: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    /// What the sidecar is doing now; recorded if the guard is dropped.
    step: SidecarStep,
    /// Set once the run has an outcome; `Drop` is then a no-op.
    ended: bool,
    /// Released after the terminal record is written, before the hooks run.
    lock: Option<LockGuard>,
}

impl SupervisedRun<Spawned> {
    /// Take ownership of a freshly spawned child. `ready` is the readiness
    /// channel if it has not been used yet (no `--after` dependencies).
    pub(super) fn adopt(
        child: SupervisedChild,
        session: SessionDir,
        lock: LockGuard,
        meta: Meta,
        lifecycle: LifecycleEvents,
        ready: Option<ReadyWriter>,
    ) -> Self {
        let identity = Current::child_identity(&child);
        let kill_handle = Current::child_kill_handle(&child);
        Self {
            core: RunCore {
                child,
                kill_handle,
                identity,
                meta,
                lifecycle,
                session,
                ready,
                stdin_errors: Arc::new(Mutex::new(Vec::new())),
                attach_sink: None,
                watch_cancel: Arc::new(AtomicBool::new(false)),
                timed_out: Arc::new(AtomicBool::new(false)),
                step: SidecarStep::Breadcrumb,
                ended: false,
                lock: Some(lock),
            },
            _phase: PhantomData,
        }
    }

    /// Set up what `Running` promises, publish it, deliver readiness, and
    /// start the timeout and kill watchers. The only way to `Running`.
    ///
    /// # Errors
    /// [`SidecarFailure`] when a step that ends the run failed (stdin
    /// transport, persisting `Running`). The child is already stopped and the
    /// failure recorded.
    pub(super) fn publish_running(mut self) -> Result<SupervisedRun<Running>, SidecarFailure> {
        self.core.publish_running()?;
        Ok(SupervisedRun {
            core: self.core,
            _phase: PhantomData,
        })
    }
}

impl SupervisedRun<Running> {
    /// Capture output until the child closes it, then observe its exit.
    ///
    /// # Errors
    /// [`SidecarFailure`] when the exit cannot be observed. The child is
    /// already stopped and the failure recorded.
    pub(super) fn supervise(&mut self) -> Result<ExitReason, SidecarFailure> {
        self.core.supervise()
    }

    /// The only normal exit: classify, record the terminal state, release the
    /// lock and run the hooks.
    ///
    /// # Errors
    /// When the terminal meta could not be written. The child has exited,
    /// the durable terminal event lets reconciliation heal meta, a copy is
    /// salvaged to lost+found, and the hooks still run.
    pub(super) fn finish(self, how: ExitReason) -> anyhow::Result<()> {
        let mut core = self.core;
        core.finish(how)
    }
}

impl RunCore {
    fn session_dir(&self) -> &Path {
        self.session.path()
    }

    fn is_pty(&self) -> bool {
        self.meta.launch_spec().io_mode == IoMode::Pty
    }

    /// End the run at `step`: stop the child, record `SidecarFailed`, and
    /// return the marker for the caller to propagate.
    fn fail(&mut self, step: SidecarStep, error: impl std::fmt::Display) -> SidecarFailure {
        self.end_failed(step, &error.to_string());
        SidecarFailure { step }
    }

    fn publish_running(&mut self) -> Result<(), SidecarFailure> {
        let dir = self.session_dir().to_path_buf();
        let is_pty = self.is_pty();

        // Recover: the breadcrumb only matters to crash recovery.
        self.step = SidecarStep::Breadcrumb;
        let breadcrumb = serde_json::to_string(&self.identity)
            .map_err(io::Error::other)
            .and_then(|json| {
                test_fault(SidecarStep::Breadcrumb.as_str())?;
                std::fs::write(dir.join("child_pid"), json)
            });
        if let Err(e) = breadcrumb {
            self.meta
                .add_warning(format!("orphan breadcrumb not written: {e}"));
        }

        // A dup'd fd for PTY resize, taken before child_stdin takes the write half.
        #[cfg(unix)]
        let resize_fd = if is_pty {
            Current::pty_resize_fd(&self.child)
        } else {
            None
        };
        // PTY input is shared by the stdin forwarder and the attach listener.
        let pty_write: Option<Arc<Mutex<Box<dyn Write + Send>>>> = if is_pty {
            Current::child_stdin(&mut self.child).map(|w| Arc::new(Mutex::new(w)))
        } else {
            None
        };

        // End: a --stdin run whose input can never arrive is a failed run.
        if self.meta.launch_spec().stdin_mode == StdinMode::Pipe {
            self.step = SidecarStep::StdinTransport;
            let child_stdin: Option<Box<dyn Write + Send>> = match &pty_write {
                Some(shared) => Some(Box::new(SharedWriter(Arc::clone(shared)))),
                None => Current::child_stdin(&mut self.child),
            };
            let transport = match child_stdin {
                Some(writer) => test_fault(SidecarStep::StdinTransport.as_str())
                    .and_then(|()| setup_stdin_forwarding(&dir, writer, &self.stdin_errors)),
                None => Err(io::Error::other("child stdin not piped")),
            };
            if let Err(e) = transport {
                return Err(self.fail(SidecarStep::StdinTransport, e));
            }
        }

        if is_pty {
            self.attach_sink = Some(Arc::new(Mutex::new(None)));
        }

        // Recover: attach is optional; bind before Running so the outcome is
        // in the published meta rather than silent.
        #[cfg(unix)]
        if is_pty {
            self.step = SidecarStep::AttachBind;
            match (&pty_write, &self.attach_sink) {
                (Some(pty_write), Some(sink)) => match bind_attach_socket(&dir) {
                    Ok(listener) => {
                        let pty_write = Arc::clone(pty_write);
                        let sink = Arc::clone(sink);
                        let facts = self.lifecycle.with_fresh_writer();
                        let session_path = dir.clone();
                        std::thread::spawn(move || {
                            super::run_attach_listener(
                                listener,
                                pty_write,
                                sink,
                                &session_path,
                                resize_fd,
                                facts,
                            );
                        });
                    }
                    Err(e) => self.meta.add_warning(format!("attach unavailable: {e}")),
                },
                _ => self
                    .meta
                    .add_warning("attach unavailable: no PTY input handle".to_owned()),
            }
        }

        // End: an unobservable Running would be a lie.
        self.step = SidecarStep::RunningMeta;
        if let Err(e) = self.meta.transition_running(self.identity) {
            return Err(self.fail(SidecarStep::RunningMeta, e));
        }
        if is_pty {
            self.meta.set_pty(PtyMeta::new());
        }
        self.lifecycle.emit(&mut self.meta, false);
        let published = test_fault(SidecarStep::RunningMeta.as_str())
            .map_err(|e| e.to_string())
            .and_then(|()| {
                session::write_meta_atomic(&self.session, &self.meta).map_err(|e| e.to_string())
            });
        if let Err(e) = published {
            return Err(self.fail(SidecarStep::RunningMeta, e));
        }

        // Recover: readiness is a courtesy to the client (#71).
        self.step = SidecarStep::Readiness;
        signal_readiness(&self.session, &mut self.ready, &mut self.meta);

        if let Some(timeout_s) = self.meta.launch_spec().timeout_s {
            self.timed_out = setup_timeout(
                self.kill_handle.clone(),
                timeout_s,
                Arc::clone(&self.watch_cancel),
            );
        }
        setup_kill_watcher(
            &dir,
            self.kill_handle.clone(),
            self.meta.run_id(),
            Arc::clone(&self.watch_cancel),
        );
        Ok(())
    }

    fn supervise(&mut self) -> Result<ExitReason, SidecarFailure> {
        // Recover: without output.log the run still has an exit worth
        // recording, but the child's output must still be read, or it blocks
        // on a full pipe. Drain it.
        self.step = SidecarStep::OutputLog;
        let log_path = self.session_dir().join("output.log");
        let opened = test_fault(SidecarStep::OutputLog.as_str())
            .and_then(|()| OpenOptions::new().create(true).append(true).open(&log_path));
        let log: Box<dyn Write + Send> = match opened {
            Ok(file) => Box::new(file),
            Err(e) => {
                self.meta.add_warning(format!(
                    "output not captured: cannot open output.log ({e}); output was drained"
                ));
                // Best effort, so the warning is visible while the run lasts;
                // the terminal write carries it regardless.
                let _ = session::write_meta_atomic(&self.session, &self.meta);
                Box::new(io::sink())
            }
        };
        self.capture_output(log);

        // End: an exit that cannot be observed cannot be classified.
        self.step = SidecarStep::ChildWait;
        let status = match test_fault(SidecarStep::ChildWait.as_str())
            .and_then(|()| Current::child_wait(&mut self.child))
        {
            Ok(status) => status,
            Err(e) => {
                return Err(self.fail(
                    SidecarStep::ChildWait,
                    format!("cannot observe the child's exit: {e}"),
                ));
            }
        };
        Ok(match status.code() {
            Some(0) => ExitReason::ExitedOk,
            Some(code) => ExitReason::ExitedError {
                code: NonZeroI32::new(code).expect("zero is matched above"),
            },
            None => ExitReason::Killed,
        })
    }

    /// Capture stdout/stderr into `log` until the child closes them. Capture
    /// errors become `capture_errors.log` (sidecar stderr is /dev/null) and
    /// never fail supervision: the child's exit status is still meaningful.
    fn capture_output(&mut self, log: Box<dyn Write + Send>) {
        let log = Mutex::new(log);
        let stdout = Current::child_stdout(&mut self.child);
        let stderr = Current::child_stderr(&mut self.child); // None for PTY sessions
        let attach_sink = self.attach_sink.as_ref();

        let log_ref = &log;
        let (stdout_result, stderr_result) = std::thread::scope(|scope| {
            let stdout_handle = stdout.map(|s| match attach_sink {
                Some(sink) => scope.spawn(move || capture_stream_with_tee(s, 'O', log_ref, sink)),
                None => scope.spawn(move || capture_stream(s, 'O', log_ref)),
            });
            let stderr_handle =
                stderr.map(|s| scope.spawn(move || capture_stream(s, 'E', log_ref)));
            let join = |handle: Option<std::thread::ScopedJoinHandle<'_, Result<(), String>>>,
                        name: &str| {
                handle
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err(format!("{name} capture thread panicked")))
                    })
                    .unwrap_or(Ok(()))
            };
            (join(stdout_handle, "stdout"), join(stderr_handle, "stderr"))
        });

        let mut capture_errors = Vec::new();
        if let Err(e) = stdout_result {
            capture_errors.push(format!("stdout capture: {e}"));
        }
        if let Err(e) = stderr_result {
            capture_errors.push(format!("stderr capture: {e}"));
        }
        if !capture_errors.is_empty() {
            let err_path = self.session_dir().join("capture_errors.log");
            let _ = std::fs::write(&err_path, capture_errors.join("\n"));
        }
    }

    fn finish(&mut self, how: ExitReason) -> anyhow::Result<()> {
        self.ended = true;
        self.watch_cancel.store(true, Ordering::Relaxed);
        let dir = self.session_dir().to_path_buf();

        let how = self.classify(how);
        self.clean_up_control_files();
        // Breadcrumb no longer needed: meta carries the child identity.
        let _ = std::fs::remove_file(dir.join("child_pid"));
        for warning in collect_warnings(&dir, &self.stdin_errors) {
            self.meta.add_warning(warning);
        }

        // WAL order: the durable terminal event precedes the terminal meta
        // write, so terminal meta always implies a logged terminal event
        // (spec §3.6).
        if let Err(e) = self.meta.transition_exited(how, EpochTimestamp::now()) {
            self.meta
                .add_warning(format!("terminal transition refused: {e}"));
        }
        test_abort_point("before_terminal_event");
        self.lifecycle.emit(&mut self.meta, true);
        test_abort_point("before_terminal_meta");
        let recorded = test_fault("terminal_meta")
            .map_err(|e| e.to_string())
            .and_then(|()| {
                session::write_meta_atomic(&self.session, &self.meta).map_err(|e| e.to_string())
            });
        if let Err(e) = &recorded {
            // The child has already exited: nothing to stop. The durable event
            // lets reconciliation heal meta; keep an independent copy too.
            self.lifecycle.salvage_unrecorded(&self.meta, e);
        }

        // Release the lock: the session is available for --replace.
        self.lock.take();
        run_on_exit_hooks(&self.meta, &dir, &mut self.lifecycle);
        recorded.map_err(|e| anyhow::anyhow!("terminal meta not written: {e}"))
    }

    /// Final exit reason. Priority: TimedOut > KilledForced > Killed (from
    /// `kill_acted`) > the raw exit.
    fn classify(&self, how: ExitReason) -> ExitReason {
        let dir = self.session_dir();
        if self.timed_out.load(Ordering::Relaxed) {
            ExitReason::TimedOut
        } else if dir.join("kill_forced").exists() {
            ExitReason::KilledForced
        } else if dir.join("kill_acted").exists() {
            // Sidecar-mediated graceful kill (force=false). The child may
            // report ExitedError on Windows (TerminateJobObject after the
            // grace period), but the user requested a kill.
            ExitReason::Killed
        } else {
            how
        }
    }

    /// Remove the run's control files and transports.
    fn clean_up_control_files(&self) {
        let dir = self.session_dir();
        for name in ["kill_forced", "kill_acted", "kill_request"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        Current::remove_stdin_transport(dir);
        if self.is_pty() {
            let _ = std::fs::remove_file(crate::attach_proto::sock_path(dir));
            let _ = std::fs::remove_file(dir.join("a.sock.path"));
        }
    }

    /// The failure path: stop the child first, then record
    /// `SidecarFailed { step }`, tell a waiting client, release the lock and
    /// run the hooks. Never claims a record it did not make.
    fn end_failed(&mut self, step: SidecarStep, error: &str) {
        if self.ended {
            return;
        }
        self.ended = true;
        self.step = step;
        self.watch_cancel.store(true, Ordering::Relaxed);

        let stopped = terminate_and_reap(&mut self.child, &self.kill_handle, &self.identity);

        let dir = self.session_dir().to_path_buf();
        self.meta
            .add_warning(format!("sidecar failed at {step}: {error}"));
        for note in &stopped.notes {
            self.meta.add_warning(note.clone());
        }
        self.clean_up_control_files();
        for warning in collect_warnings(&dir, &self.stdin_errors) {
            self.meta.add_warning(warning);
        }

        if let Err(e) =
            self.meta
                .transition_sidecar_failed(self.identity, step, EpochTimestamp::now())
        {
            self.meta
                .add_warning(format!("SidecarFailed transition refused: {e}"));
        }
        self.lifecycle.emit(&mut self.meta, true);
        let recorded = test_fault("failure_record")
            .map_err(|e| e.to_string())
            .and_then(|()| {
                session::write_meta_atomic(&self.session, &self.meta).map_err(|e| e.to_string())
            });

        match &recorded {
            Ok(()) => {
                if stopped.gone {
                    let _ = std::fs::remove_file(dir.join("child_pid"));
                }
                // A client still waiting gets the terminal snapshot.
                signal_readiness(&self.session, &mut self.ready, &mut self.meta);
            }
            Err(record_error) => {
                // Loud on every channel still open. The child_pid breadcrumb
                // stays, so a `start` client keeps the directory and later
                // recovery can still find the child.
                let message = format!(
                    "sidecar failed at {step}: {error}; {}; recording the failure also failed: {record_error}",
                    stopped.summary()
                );
                self.lifecycle.salvage_unrecorded(&self.meta, record_error);
                eprintln!("tender sidecar: {message}");
                if let Some(writer) = self.ready.take() {
                    let _ = Current::write_ready_signal(writer, &format!("ERROR:{message}\n"));
                }
            }
        }

        self.lock.take();
        run_on_exit_hooks(&self.meta, &dir, &mut self.lifecycle);
    }
}

impl Drop for RunCore {
    /// Reached only when the run was abandoned: an early return, a `?`, or a
    /// panic unwinding through the guard.
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        let step = self.step;
        let error = if std::thread::panicking() {
            format!("panicked: {}", super::last_panic_message())
        } else {
            "supervision ended without an outcome".to_owned()
        };
        if std::thread::panicking() {
            // A second panic here would abort; end_failed does not panic.
            self.end_failed(step, &error);
        } else {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| self.end_failed(step, &error)));
        }
    }
}

/// Bind the PTY attach socket (replacing a stale one) and publish its path.
#[cfg(unix)]
fn bind_attach_socket(session_dir: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    let sock_path = crate::attach_proto::sock_path(session_dir);
    test_fault(SidecarStep::AttachBind.as_str())?;
    let _ = std::fs::remove_file(&sock_path);
    let listener = std::os::unix::net::UnixListener::bind(&sock_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot bind {}: {e}", sock_path.display()),
        )
    })?;
    crate::attach_proto::write_sock_breadcrumb(session_dir, &sock_path);
    Ok(listener)
}

/// What stopping the child achieved.
struct Stopped {
    /// The child has exited (reaped, or verifiably gone).
    gone: bool,
    /// Warnings for meta: kill errors, or a child that would not die.
    notes: Vec<String>,
}

impl Stopped {
    fn summary(&self) -> &'static str {
        if self.gone {
            "the child was stopped"
        } else {
            "the child may still be running"
        }
    }
}

/// Stop the child through the platform kill path (process group on Unix, Job
/// Object on Windows): graceful first, forced after the grace period. The
/// graceful kill runs on its own thread while this one reaps, because on Unix
/// an unreaped child stays a zombie that the kill's liveness probe still sees.
/// Panic-free: this runs from `Drop` during unwinding, where a second panic
/// would abort before the child is stopped. If no thread can be created, the
/// child is force-killed inline instead.
fn terminate_and_reap(
    child: &mut SupervisedChild,
    handle: &ChildKillHandle,
    identity: &ProcessIdentity,
) -> Stopped {
    let graceful = handle.clone();
    let killer = std::thread::Builder::new()
        .name("tender-stop-child".to_owned())
        .spawn(move || Current::kill_child(&graceful, false));
    let mut notes = Vec::new();
    let killer = match killer {
        Ok(killer) => Some(killer),
        Err(e) => {
            notes.push(format!(
                "no thread for a graceful stop ({e}); force-killing"
            ));
            if let Err(e) = Current::kill_child(handle, true) {
                notes.push(format!("force-killing the child failed: {e}"));
            }
            None
        }
    };

    let mut gone = wait_gone(child, identity, Duration::from_secs(10));
    if !gone {
        if let Err(e) = Current::kill_child(handle, true) {
            notes.push(format!("force-killing the child failed: {e}"));
        }
        gone = wait_gone(child, identity, Duration::from_secs(2));
    }
    match killer.map(std::thread::JoinHandle::join) {
        None | Some(Ok(Ok(()))) => {}
        Some(Ok(Err(e))) => notes.push(format!("stopping the child failed: {e}")),
        Some(Err(_)) => notes.push("stopping the child panicked".to_owned()),
    }
    if !gone {
        notes.push(format!(
            "child pid {} may still be running: it outlived a forced kill",
            identity.pid
        ));
    }
    Stopped { gone, notes }
}

/// Reap the child, or confirm it is gone, within `timeout`.
fn wait_gone(child: &mut SupervisedChild, identity: &ProcessIdentity, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match Current::child_try_wait(child) {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            // Cannot reap (e.g. wait itself is failing): fall back to probing.
            Err(_) => {
                if matches!(
                    Current::process_status(identity),
                    ProcessStatus::Missing | ProcessStatus::IdentityMismatch
                ) {
                    return true;
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
