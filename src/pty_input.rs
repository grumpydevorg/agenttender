//! The sidecar's PTY input authority: one writer thread per PTY run.
//!
//! The thread exclusively owns the run's [`InputArbiter`] and the PTY master's
//! write half. Every other thread — attach connections, the push forwarder —
//! talks to it through [`InputWriter`], which carries two bounded queues:
//!
//! - **controls** (claim, takeover, release, resize) are always served before
//!   the next input step, so a full PTY input buffer or a long queued push can
//!   never delay a takeover;
//! - **inputs** (write requests and end-of-input markers) are served in order,
//!   one nonblocking write attempt at a time, and apply backpressure to their
//!   senders when full.
//!
//! Authorization and each write happen inside [`InputArbiter::write_step`] on
//! this thread, so no takeover can land between the check and the write. A
//! request queued by a controller that has since been superseded writes nothing
//! further. Contract: [`crate::model::pty_control`].

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::model::ids::RunId;
use crate::model::pty_control::{
    ControlError, ControllerEpoch, ControllerHandle, ControllerKind, ControllerState, HolderId,
    IncompleteReason, InputArbiter, InputOutcome, PendingInput, PtyInputSink, RequestId, Takeover,
    WriteStep,
};
use crate::recording::Geometry;

/// Maximum queued control requests before senders wait.
const CONTROL_CAPACITY: usize = 16;
/// Maximum queued input items before senders wait.
const INPUT_CAPACITY: usize = 32;
/// How long the writer waits for writability before rechecking controls.
const WRITABLE_POLL: Duration = Duration::from_millis(10);

/// A PTY input the writer thread can drive without blocking.
pub trait WritablePty: PtyInputSink + Send + 'static {
    /// Wait up to `timeout` for the PTY to accept input.
    ///
    /// # Errors
    ///
    /// The descriptor's poll error.
    fn wait_writable(&self, timeout: Duration) -> io::Result<bool>;

    /// Apply a window size to the PTY.
    fn resize(&self, size: Geometry);
}

/// Side effects of ownership changes, run on the writer thread. Implementations
/// must return promptly; anything that can block belongs on another thread.
pub trait ControlHooks: Send + 'static {
    /// A human took control (`trigger` is `"attach"` or `"takeover"`).
    fn human_control(&mut self, trigger: &'static str);
    /// No human holds control any more.
    fn human_released(&mut self);
    /// `holder` was superseded by a takeover at `epoch` and must be retired.
    fn retire(&mut self, holder: HolderId, kind: ControllerKind, epoch: ControllerEpoch);
    /// A request from a superseded controller was stopped.
    fn input_revoked(&mut self, kind: ControllerKind, accepted: usize, total: usize);
}

enum Control {
    Claim {
        holder: HolderId,
        kind: ControllerKind,
        reply: SyncSender<Result<ControllerHandle, ControlError>>,
    },
    Takeover {
        holder: HolderId,
        reply: SyncSender<Result<Takeover, ControlError>>,
    },
    Resize {
        handle: ControllerHandle,
        size: Geometry,
    },
    /// Release now and cancel everything the holder still has queued.
    Disconnect { handle: ControllerHandle },
}

enum InputItem {
    Write {
        request: PendingInput,
        reply: Option<SyncSender<InputOutcome>>,
    },
    End {
        handle: ControllerHandle,
        /// Receives whether the holder still held control and was released.
        done: Option<SyncSender<bool>>,
    },
}

#[derive(Default)]
struct Queues {
    controls: VecDeque<Control>,
    inputs: VecDeque<InputItem>,
}

struct Shared {
    queues: Mutex<Queues>,
    /// Signals the writer that work arrived.
    work: Condvar,
    /// Signals senders that queue space was freed.
    space: Condvar,
    next_holder: AtomicU64,
    next_request: AtomicU64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queues> {
        self.queues.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Why a control request did not take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error(transparent)]
    Control(#[from] ControlError),
    #[error("the PTY input writer is no longer running")]
    WriterGone,
}

/// Cloneable sender side of a PTY run's input authority.
#[derive(Clone)]
pub struct InputWriter {
    shared: Arc<Shared>,
}

impl InputWriter {
    /// Start the writer thread for `run_id`, owning `pty`.
    pub fn spawn<P: WritablePty, H: ControlHooks>(run_id: RunId, pty: P, hooks: H) -> Self {
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::default()),
            work: Condvar::new(),
            space: Condvar::new(),
            next_holder: AtomicU64::new(1),
            next_request: AtomicU64::new(1),
        });
        let thread_shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            writer_loop(&thread_shared, InputArbiter::new(run_id), pty, hooks)
        });
        Self { shared }
    }

    /// A fresh holder identity for a connection or claim.
    #[must_use]
    pub fn next_holder(&self) -> HolderId {
        HolderId::new(self.shared.next_holder.fetch_add(1, Ordering::Relaxed))
    }

    /// Take control of an unowned PTY.
    ///
    /// # Errors
    ///
    /// [`ControlError::Busy`] if another controller holds it;
    /// [`RequestError::WriterGone`] if the writer thread has stopped.
    pub fn claim(
        &self,
        holder: HolderId,
        kind: ControllerKind,
    ) -> Result<ControllerHandle, RequestError> {
        let (reply, rx) = sync_channel(1);
        self.push_control(Control::Claim {
            holder,
            kind,
            reply,
        });
        Ok(rx.recv().map_err(|_| RequestError::WriterGone)??)
    }

    /// Human takeover, retiring any current controller.
    ///
    /// # Errors
    ///
    /// [`ControlError::EpochExhausted`]; [`RequestError::WriterGone`].
    pub fn takeover(&self, holder: HolderId) -> Result<Takeover, RequestError> {
        let (reply, rx) = sync_channel(1);
        self.push_control(Control::Takeover { holder, reply });
        Ok(rx.recv().map_err(|_| RequestError::WriterGone)??)
    }

    /// Apply a window size if `handle` still authorizes.
    pub fn resize(&self, handle: ControllerHandle, size: Geometry) {
        self.push_control(Control::Resize { handle, size });
    }

    /// Queue `bytes` and wait for the outcome. Empty input is a no-op.
    pub fn write(&self, handle: ControllerHandle, bytes: Vec<u8>) -> Option<InputOutcome> {
        self.submit(handle, bytes)?.recv().ok()
    }

    /// Queue `bytes` (waiting only for queue space) and return where the outcome
    /// will arrive, so the caller can watch other events while it waits. `None`
    /// for empty input. A closed receiver means the writer stopped.
    pub fn submit(
        &self,
        handle: ControllerHandle,
        bytes: Vec<u8>,
    ) -> Option<std::sync::mpsc::Receiver<InputOutcome>> {
        let request = self.request(handle, bytes)?;
        let (reply, rx) = sync_channel(1);
        self.push_input(InputItem::Write {
            request,
            reply: Some(reply),
        });
        Some(rx)
    }

    /// Queue `bytes` without waiting for the outcome (waits only for queue space).
    pub fn write_nowait(&self, handle: ControllerHandle, bytes: Vec<u8>) {
        if let Some(request) = self.request(handle, bytes) {
            self.push_input(InputItem::Write {
                request,
                reply: None,
            });
        }
    }

    /// Like [`InputWriter::write_nowait`], but give up waiting for queue space
    /// as soon as `abandon` returns `true` (checked at least every 50 ms), so a
    /// sender whose client has gone is not stuck behind a full queue. Returns
    /// `false` if abandoned; nothing was queued then.
    pub fn write_nowait_unless(
        &self,
        handle: ControllerHandle,
        bytes: Vec<u8>,
        abandon: impl Fn() -> bool,
    ) -> bool {
        let Some(request) = self.request(handle, bytes) else {
            return true;
        };
        let mut queues = self.shared.lock();
        while queues.inputs.len() >= INPUT_CAPACITY {
            if abandon() {
                return false;
            }
            queues = self
                .shared
                .space
                .wait_timeout(queues, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        queues.inputs.push_back(InputItem::Write {
            request,
            reply: None,
        });
        drop(queues);
        self.shared.work.notify_one();
        true
    }

    /// Release `handle` once every input it queued before this call is done.
    /// A handle that no longer authorizes releases nothing.
    pub fn end_of_input(&self, handle: ControllerHandle) {
        self.push_input(InputItem::End { handle, done: None });
    }

    /// The controller behind `handle` disconnected: release it now, ahead of any
    /// queued input, and cancel everything it still has queued or in flight.
    /// Unlike [`InputWriter::end_of_input`], nothing it queued is written after
    /// this is served, so a full PTY cannot keep a vanished client in control.
    pub fn disconnect(&self, handle: ControllerHandle) {
        self.push_control(Control::Disconnect { handle });
    }

    /// Like [`InputWriter::end_of_input`], but wait for the release. `true` only
    /// if `handle` still held control when its input ended, so every byte it
    /// queued before this call was authorized; `false` if it had been superseded
    /// or its input was cancelled.
    pub fn end_of_input_and_wait(&self, handle: ControllerHandle) -> bool {
        let (done, rx) = sync_channel(1);
        self.push_input(InputItem::End {
            handle,
            done: Some(done),
        });
        rx.recv().unwrap_or(false)
    }

    fn request(&self, handle: ControllerHandle, bytes: Vec<u8>) -> Option<PendingInput> {
        let id = RequestId::new(self.shared.next_request.fetch_add(1, Ordering::Relaxed));
        PendingInput::new(id, handle, bytes).ok()
    }

    fn push_control(&self, control: Control) {
        let mut queues = self.shared.lock();
        while queues.controls.len() >= CONTROL_CAPACITY {
            queues = self
                .shared
                .space
                .wait(queues)
                .unwrap_or_else(|e| e.into_inner());
        }
        queues.controls.push_back(control);
        drop(queues);
        self.shared.work.notify_one();
    }

    fn push_input(&self, item: InputItem) {
        let mut queues = self.shared.lock();
        while queues.inputs.len() >= INPUT_CAPACITY {
            queues = self
                .shared
                .space
                .wait(queues)
                .unwrap_or_else(|e| e.into_inner());
        }
        queues.inputs.push_back(item);
        drop(queues);
        self.shared.work.notify_one();
    }
}

/// The request being written, with where to report its outcome.
struct Current {
    request: PendingInput,
    reply: Option<SyncSender<InputOutcome>>,
}

fn writer_loop<P: WritablePty, H: ControlHooks>(
    shared: &Shared,
    mut arbiter: InputArbiter,
    mut pty: P,
    mut hooks: H,
) {
    let mut kinds: HashMap<HolderId, ControllerKind> = HashMap::new();
    let mut current: Option<Current> = None;

    loop {
        // Take all controls, and the next input if nothing is being written.
        let (controls, next_input) = {
            let mut queues = shared.lock();
            while queues.controls.is_empty() && queues.inputs.is_empty() && current.is_none() {
                queues = shared.work.wait(queues).unwrap_or_else(|e| e.into_inner());
            }
            let controls: Vec<Control> = queues.controls.drain(..).collect();
            let next_input = if current.is_none() {
                queues.inputs.pop_front()
            } else {
                None
            };
            (controls, next_input)
        };
        shared.space.notify_all();

        // Stage the dequeued item first, so a disconnect served below can cancel
        // it along with everything else its holder queued.
        let mut next_end = None;
        match next_input {
            Some(InputItem::Write { request, reply }) => current = Some(Current { request, reply }),
            Some(end @ InputItem::End { .. }) => next_end = Some(end),
            None => {}
        }

        for control in controls {
            if let Control::Disconnect { handle } = control {
                disconnect(
                    handle,
                    shared,
                    &mut arbiter,
                    &mut kinds,
                    &mut pty,
                    &mut hooks,
                    &mut current,
                    &mut next_end,
                );
            } else {
                apply_control(control, &mut arbiter, &mut kinds, &pty, &mut hooks);
            }
        }

        if let Some(InputItem::End { handle, done }) = next_end {
            let released = release(&mut arbiter, &mut kinds, handle, &mut hooks);
            if let Some(done) = done {
                let _ = done.send(released);
            }
            continue;
        }

        let Some(mut writing) = current.take() else {
            continue;
        };
        let before = writing.request.accepted();
        match arbiter.write_step(&mut writing.request, &mut pty) {
            WriteStep::Done(outcome) => {
                if let InputOutcome::Incomplete {
                    accepted,
                    total,
                    reason: IncompleteReason::NotAuthorized(_),
                    ..
                } = outcome
                {
                    let holder = writing.request.handle().holder();
                    let kind = kinds.get(&holder).copied().unwrap_or(ControllerKind::Agent);
                    hooks.input_revoked(kind, accepted, total);
                }
                if let Some(reply) = writing.reply {
                    let _ = reply.send(outcome);
                }
            }
            WriteStep::Pending => {
                if writing.request.accepted() == before {
                    // No progress: the PTY input buffer is full. Wait briefly for
                    // room, then loop to serve any control that arrived.
                    let _ = pty.wait_writable(WRITABLE_POLL);
                }
                current = Some(writing);
            }
        }
    }
}

/// Serve a disconnect ahead of all input: release `handle` if it is current,
/// then cancel every input its holder still has staged, queued, or in flight.
///
/// Each cancelled write is finished through [`InputArbiter::write_step`], which
/// cannot reach the PTY once the handle no longer authorizes, so callers waiting
/// on an outcome receive `NotAuthorized` with the exact accepted count. The
/// holder's revoked bytes are reported once, in aggregate.
#[allow(clippy::too_many_arguments)]
fn disconnect<P: WritablePty, H: ControlHooks>(
    handle: ControllerHandle,
    shared: &Shared,
    arbiter: &mut InputArbiter,
    kinds: &mut HashMap<HolderId, ControllerKind>,
    pty: &mut P,
    hooks: &mut H,
    current: &mut Option<Current>,
    next_end: &mut Option<InputItem>,
) {
    let holder = handle.holder();
    let kind = kinds.get(&holder).copied();
    let _ = release(arbiter, kinds, handle, hooks);

    let belongs = |item: &InputItem| match item {
        InputItem::Write { request, .. } => request.handle().holder() == holder,
        InputItem::End { handle, .. } => handle.holder() == holder,
    };
    let mut cancelled: Vec<InputItem> = {
        let mut queues = shared.lock();
        let (theirs, others): (VecDeque<_>, VecDeque<_>) =
            queues.inputs.drain(..).partition(|item| belongs(item));
        queues.inputs = others;
        theirs.into_iter().collect()
    };
    shared.space.notify_all();
    if next_end.as_ref().is_some_and(belongs) {
        cancelled.extend(next_end.take());
    }
    if current
        .as_ref()
        .is_some_and(|c| c.request.handle().holder() == holder)
    {
        if let Some(Current { request, reply }) = current.take() {
            cancelled.push(InputItem::Write { request, reply });
        }
    }

    let (mut accepted_total, mut bytes_total) = (0, 0);
    for item in cancelled {
        match item {
            InputItem::Write { mut request, reply } => {
                if let WriteStep::Done(outcome) = arbiter.write_step(&mut request, pty) {
                    if let InputOutcome::Incomplete {
                        accepted, total, ..
                    } = outcome
                    {
                        accepted_total += accepted;
                        bytes_total += total;
                    }
                    if let Some(reply) = reply {
                        let _ = reply.send(outcome);
                    }
                }
            }
            InputItem::End { done, .. } => {
                if let Some(done) = done {
                    // Cancelled by a disconnect: its input did not complete.
                    let _ = done.send(false);
                }
            }
        }
    }
    if bytes_total > 0 {
        hooks.input_revoked(
            kind.unwrap_or(ControllerKind::Human),
            accepted_total,
            bytes_total,
        );
    }
}

fn apply_control<P: WritablePty, H: ControlHooks>(
    control: Control,
    arbiter: &mut InputArbiter,
    kinds: &mut HashMap<HolderId, ControllerKind>,
    pty: &P,
    hooks: &mut H,
) {
    match control {
        Control::Claim {
            holder,
            kind,
            reply,
        } => {
            let result = arbiter.claim(holder, kind);
            if result.is_ok() {
                kinds.insert(holder, kind);
                if kind == ControllerKind::Human {
                    hooks.human_control("attach");
                }
            }
            let _ = reply.send(result);
        }
        Control::Takeover { holder, reply } => {
            let result = arbiter.takeover(holder);
            if let Ok(takeover) = &result {
                kinds.insert(holder, ControllerKind::Human);
                hooks.human_control("takeover");
                if let Some((retired, kind)) = takeover.retired {
                    hooks.retire(retired, kind, takeover.handle.epoch());
                }
            }
            let _ = reply.send(result);
        }
        Control::Resize { handle, size } => {
            if arbiter.authorize(&handle).is_ok() {
                pty.resize(size);
            }
        }
        Control::Disconnect { .. } => {
            debug_assert!(
                false,
                "disconnect needs the queues; the writer loop serves it"
            );
        }
    }
}

/// End of a holder's input: release if it still authorizes, and forget the
/// holder either way (it queues nothing after its end marker). Returns whether
/// the holder was still current and has now been released.
fn release<H: ControlHooks>(
    arbiter: &mut InputArbiter,
    kinds: &mut HashMap<HolderId, ControllerKind>,
    handle: ControllerHandle,
    hooks: &mut H,
) -> bool {
    let human_owned = matches!(
        arbiter.state(),
        ControllerState::Owned {
            kind: ControllerKind::Human,
            ..
        }
    );
    // A successful release proves `handle` was current, so `human_owned` then
    // describes the holder that just released.
    let released = arbiter.release(&handle).is_ok();
    kinds.remove(&handle.holder());
    if released && human_owned {
        hooks.human_released();
    }
    released
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A PTY whose input buffer is permanently full: the child never reads.
    struct FullPty;

    impl PtyInputSink for FullPty {
        fn try_write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }

    impl WritablePty for FullPty {
        fn wait_writable(&self, timeout: Duration) -> io::Result<bool> {
            std::thread::sleep(timeout.min(Duration::from_millis(1)));
            Ok(false)
        }

        fn resize(&self, _size: Geometry) {}
    }

    /// A PTY that accepts one byte per attempt and records everything written.
    #[derive(Clone, Default)]
    struct SlowPty(Arc<Mutex<Vec<u8>>>);

    impl PtyInputSink for SlowPty {
        fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().push(bytes[0]);
            Ok(1)
        }
    }

    impl WritablePty for SlowPty {
        fn wait_writable(&self, _timeout: Duration) -> io::Result<bool> {
            Ok(true)
        }

        fn resize(&self, _size: Geometry) {}
    }

    /// Records hook calls in order.
    #[derive(Clone, Default)]
    struct Recorded(Arc<Mutex<Vec<String>>>);

    impl Recorded {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl ControlHooks for Recorded {
        fn human_control(&mut self, trigger: &'static str) {
            self.0
                .lock()
                .unwrap()
                .push(format!("human_control:{trigger}"));
        }

        fn human_released(&mut self) {
            self.0.lock().unwrap().push("human_released".to_owned());
        }

        fn retire(&mut self, holder: HolderId, kind: ControllerKind, epoch: ControllerEpoch) {
            self.0
                .lock()
                .unwrap()
                .push(format!("retire:{}:{kind:?}:{epoch}", holder.get()));
        }

        fn input_revoked(&mut self, kind: ControllerKind, accepted: usize, total: usize) {
            self.0
                .lock()
                .unwrap()
                .push(format!("input_revoked:{kind:?}:{accepted}:{total}"));
        }
    }

    #[test]
    fn disconnect_releases_ahead_of_input_stuck_behind_a_full_pty() {
        let hooks = Recorded::default();
        let writer = InputWriter::spawn(RunId::new(), FullPty, hooks.clone());
        let human = writer
            .claim(writer.next_holder(), ControllerKind::Human)
            .unwrap();
        for _ in 0..4 {
            writer.write_nowait(human, vec![b'x'; 1024]);
        }
        let stuck = {
            let writer = writer.clone();
            std::thread::spawn(move || writer.write(human, b"never-written".to_vec()))
        };

        writer.disconnect(human);

        // Control is free again even though the PTY never accepts input.
        let deadline = Instant::now() + Duration::from_secs(5);
        let next = loop {
            match writer.claim(writer.next_holder(), ControllerKind::Human) {
                Ok(handle) => break handle,
                Err(e) => assert!(
                    Instant::now() < deadline,
                    "a disconnected client still holds control: {e}"
                ),
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(next.epoch() > human.epoch());

        // The disconnected client's in-flight write is cancelled, not written.
        match stuck.join().unwrap() {
            Some(InputOutcome::Incomplete {
                accepted: 0,
                reason: IncompleteReason::NotAuthorized(_),
                ..
            }) => {}
            other => panic!("the stuck write must be revoked, got {other:?}"),
        }
        assert!(hooks.calls().contains(&"human_released".to_owned()));
    }

    #[test]
    fn end_of_input_still_drains_before_release() {
        let pty = SlowPty::default();
        let writer = InputWriter::spawn(RunId::new(), pty.clone(), Recorded::default());
        let agent = writer
            .claim(writer.next_holder(), ControllerKind::Agent)
            .unwrap();
        writer.write_nowait(agent, b"first".to_vec());
        writer.write_nowait(agent, b"second".to_vec());

        assert!(
            writer.end_of_input_and_wait(agent),
            "released while current"
        );

        assert_eq!(pty.0.lock().unwrap().as_slice(), b"firstsecond");
        assert!(
            writer
                .claim(writer.next_holder(), ControllerKind::Agent)
                .is_ok(),
            "released once its input was written"
        );
    }

    /// Records applied window sizes; accepts all input.
    #[derive(Clone, Default)]
    struct SizedPty(Arc<Mutex<Vec<Geometry>>>);

    impl PtyInputSink for SizedPty {
        fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
    }

    impl WritablePty for SizedPty {
        fn wait_writable(&self, _timeout: Duration) -> io::Result<bool> {
            Ok(true)
        }

        fn resize(&self, size: Geometry) {
            self.0.lock().unwrap().push(size);
        }
    }

    #[test]
    fn only_the_current_controller_can_resize() {
        let pty = SizedPty::default();
        let writer = InputWriter::spawn(RunId::new(), pty.clone(), Recorded::default());
        let old = writer
            .claim(writer.next_holder(), ControllerKind::Human)
            .unwrap();
        let new = writer.takeover(writer.next_holder()).unwrap().handle;

        writer.resize(old, Geometry::new(10, 20).unwrap());
        writer.resize(new, Geometry::new(30, 40).unwrap());
        // Controls are served in order; a claim round-trip proves both resizes
        // were processed before asserting.
        let _ = writer.claim(writer.next_holder(), ControllerKind::Agent);

        assert_eq!(
            pty.0.lock().unwrap().as_slice(),
            &[Geometry::new(30, 40).unwrap()]
        );
    }

    #[test]
    fn end_of_input_reports_a_superseded_holder_as_not_released() {
        let writer = InputWriter::spawn(RunId::new(), SlowPty::default(), Recorded::default());
        let agent = writer
            .claim(writer.next_holder(), ControllerKind::Agent)
            .unwrap();
        writer.takeover(writer.next_holder()).unwrap();

        assert!(
            !writer.end_of_input_and_wait(agent),
            "a push whose control was taken over must not be reported complete"
        );
    }
}
