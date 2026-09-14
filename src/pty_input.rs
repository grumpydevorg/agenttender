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
    fn resize(&self, rows: u16, cols: u16);
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
        rows: u16,
        cols: u16,
    },
}

enum InputItem {
    Write {
        request: PendingInput,
        reply: Option<SyncSender<InputOutcome>>,
    },
    End {
        handle: ControllerHandle,
        done: Option<SyncSender<()>>,
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
    pub fn resize(&self, handle: ControllerHandle, rows: u16, cols: u16) {
        self.push_control(Control::Resize { handle, rows, cols });
    }

    /// Queue `bytes` and wait for the outcome. Empty input is a no-op.
    pub fn write(&self, handle: ControllerHandle, bytes: Vec<u8>) -> Option<InputOutcome> {
        let request = self.request(handle, bytes)?;
        let (reply, rx) = sync_channel(1);
        self.push_input(InputItem::Write {
            request,
            reply: Some(reply),
        });
        rx.recv().ok()
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

    /// Release `handle` once every input it queued before this call is done.
    /// A handle that no longer authorizes releases nothing.
    pub fn end_of_input(&self, handle: ControllerHandle) {
        self.push_input(InputItem::End { handle, done: None });
    }

    /// Like [`InputWriter::end_of_input`], but wait until the release happened.
    pub fn end_of_input_and_wait(&self, handle: ControllerHandle) {
        let (done, rx) = sync_channel(1);
        self.push_input(InputItem::End {
            handle,
            done: Some(done),
        });
        let _ = rx.recv();
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

        for control in controls {
            apply_control(control, &mut arbiter, &mut kinds, &pty, &mut hooks);
        }

        match next_input {
            Some(InputItem::Write { request, reply }) => current = Some(Current { request, reply }),
            Some(InputItem::End { handle, done }) => {
                release(&mut arbiter, &mut kinds, handle, &mut hooks);
                if let Some(done) = done {
                    let _ = done.send(());
                }
                continue;
            }
            None => {}
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
        Control::Resize { handle, rows, cols } => {
            if arbiter.authorize(&handle).is_ok() {
                pty.resize(rows, cols);
            }
        }
    }
}

/// End of a holder's input: release if it still authorizes, and forget the
/// holder either way (it queues nothing after its end marker).
fn release<H: ControlHooks>(
    arbiter: &mut InputArbiter,
    kinds: &mut HashMap<HolderId, ControllerKind>,
    handle: ControllerHandle,
    hooks: &mut H,
) {
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
}
