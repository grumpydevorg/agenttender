use tendr::attach_escape::EscapeMode;
#[cfg(unix)]
use tendr::attach_proto;
use tendr::model::ids::{Namespace, SessionName};
use tendr::model::pty::PtyControl;
use tendr::model::state::RunStatus;
use tendr::session::{self, SessionRoot};

pub fn cmd_attach(
    name: &str,
    namespace: &Namespace,
    takeover: bool,
    escape: EscapeMode,
) -> anyhow::Result<()> {
    let session_name = SessionName::new(name)?;
    let root = SessionRoot::default_path()?;

    let session = session::open(&root, namespace, &session_name)?
        .ok_or_else(|| anyhow::anyhow!("session not found: {name}"))?;

    let meta = session::read_meta(&session)?;

    if !matches!(meta.status(), RunStatus::Running { .. }) {
        anyhow::bail!("session is not running");
    }

    let pty = meta
        .pty()
        .ok_or_else(|| anyhow::anyhow!("session is not PTY-enabled"))?;

    // Informational pre-check for a clear message; the sidecar is the authority.
    if !takeover && pty.control == PtyControl::HumanControl {
        anyhow::bail!("session is already under human control (use --takeover to take it over)");
    }

    #[cfg(not(unix))]
    {
        let _ = (takeover, escape);
        anyhow::bail!("attach is only supported on Unix");
    }

    #[cfg(unix)]
    {
        let sock_path = attach_proto::read_sock_path(session.path())
            .ok_or_else(|| anyhow::anyhow!("attach socket not found"))?;

        // Fail before any side effect — a takeover included — if there is no
        // terminal to drive.
        if !rustix::termios::isatty(std::io::stdin()) {
            anyhow::bail!("attach requires an interactive terminal on stdin");
        }

        let mut stream = std::os::unix::net::UnixStream::connect(&sock_path)?;
        // Keystrokes go only to a listener run by this same user: a spoofed
        // socket owned by someone else is refused before the hello.
        tendr::attach_socket::verify_peer(&stream)
            .map_err(|e| anyhow::anyhow!("refusing attach socket {}: {e}", sock_path.display()))?;
        handshake(&mut stream, takeover)?;
        unix_relay::relay(stream, escape)
    }
}

/// Send the v1 hello and require the sidecar's acceptance.
#[cfg(unix)]
fn handshake(stream: &mut std::os::unix::net::UnixStream, takeover: bool) -> anyhow::Result<()> {
    use attach_proto::{Frame, Mode};

    let mode = if takeover {
        Mode::Takeover
    } else {
        Mode::Attach
    };
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    attach_proto::write_frame(stream, &Frame::Hello(mode))?;
    match attach_proto::read_frame(stream) {
        Ok(Frame::Accepted(_)) => {}
        Ok(Frame::Rejected(reason)) => anyhow::bail!("attach rejected: {reason}"),
        Ok(other) => anyhow::bail!("unexpected attach reply {other:?}"),
        Err(e) => anyhow::bail!(
            "the session did not complete the attach handshake ({e}); it may predate attach protocol v1"
        ),
    }
    stream.set_read_timeout(None)?;
    Ok(())
}

/// The interactive relay between the user's terminal and the session.
///
/// Three independent paths, so no blocked one can delay a detach:
///
/// - the **main thread** polls the keyboard, recognizes the escape, watches for
///   terminal size changes, and only ever queues messages;
/// - a **sender thread** writes queued messages to the session, controls
///   (resize, detach) before keystrokes; if the session stops accepting input,
///   only this thread waits;
/// - a **reader thread** writes session output to the terminal; if the terminal
///   stops reading, only this thread waits.
///
/// Leaving never joins a blocked thread: the socket is shut down, the terminal
/// is restored (without waiting for pending output to drain), and the process
/// exits.
#[cfg(unix)]
mod unix_relay {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use tendr::attach_escape::{Escape, EscapeMode, EscapeParser};
    use tendr::attach_proto::{self, Frame, FrameError};

    /// Keystrokes that may wait for a session that is not accepting input.
    /// Beyond this, further keystrokes are dropped (and reported) rather than
    /// buffered without bound; the escape is still recognized.
    const OUTBOX_DATA_LIMIT: usize = 1 << 20;
    /// How often the keyboard and terminal size are checked.
    const POLL: Duration = Duration::from_millis(100);
    /// How long a detach waits for the detach message to be sent.
    const DETACH_GRACE: Duration = Duration::from_millis(200);

    pub fn relay(stream: UnixStream, escape: EscapeMode) -> anyhow::Result<()> {
        let shutdown = stream.try_clone()?;
        let reader_stream = stream.try_clone()?;
        let closed = Arc::new(AtomicBool::new(false));
        let retired = Arc::new(AtomicBool::new(false));
        let outbox = Arc::new(Outbox::default());

        // Restores the terminal on every way out of this function, panics
        // included.
        let terminal = RawTerminal::enter()?;

        spawn_sender(stream, Arc::clone(&outbox), Arc::clone(&closed));
        spawn_reader(reader_stream, Arc::clone(&closed), Arc::clone(&retired));

        let mut size = terminal_size();
        if let Some(size) = size {
            outbox.push_control(Frame::Resize(size));
        }

        let mut parser = EscapeParser::new(escape);
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        let mut dropped = 0usize;
        let ending = loop {
            if closed.load(Ordering::SeqCst) {
                break Ending::SessionClosed;
            }
            let now = terminal_size();
            if now.is_some() && now != size {
                size = now;
                if let Some(size) = now {
                    outbox.push_control(Frame::Resize(size));
                }
            }
            if !stdin_readable(POLL) {
                continue;
            }
            let n = match stdin.read(&mut buf) {
                Ok(0) => break Ending::KeyboardClosed,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break Ending::KeyboardClosed,
            };
            let mut forward = Vec::new();
            let decision = parser.feed(&buf[..n], &mut forward);
            if !forward.is_empty() {
                let len = forward.len();
                if !outbox.push_data(forward) {
                    dropped += len;
                }
            }
            if decision == Escape::Detach {
                break Ending::Detached;
            }
        };

        if ending == Ending::Detached {
            outbox.push_control(Frame::Detach);
            outbox.wait_controls_sent(DETACH_GRACE);
        }
        // Unblocks the sender and reader on the socket; the session treats a
        // closed connection as a detach.
        let _ = shutdown.shutdown(std::net::Shutdown::Both);
        outbox.close();
        drop(terminal);

        // Only report once the terminal is restored, and never after a user
        // detach, whose terminal may be the thing that is blocked.
        if ending != Ending::Detached {
            if retired.load(Ordering::SeqCst) {
                eprintln!("tendr: another client took over this session");
            }
            if dropped > 0 {
                eprintln!(
                    "tendr: {dropped} typed bytes were dropped while the session was not accepting input"
                );
            }
        }
        Ok(())
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ending {
        Detached,
        SessionClosed,
        KeyboardClosed,
    }

    /// Raw mode for the user's terminal, restored on drop.
    struct RawTerminal {
        original: libc::termios,
    }

    impl RawTerminal {
        fn enter() -> anyhow::Result<Self> {
            use std::os::unix::io::AsRawFd;
            let fd = std::io::stdin().as_raw_fd();
            // SAFETY: termios is plain old data; tcgetattr fills it completely.
            let mut original: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: `fd` is this process's stdin; `original` is a valid out-pointer.
            if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
                anyhow::bail!("failed to get terminal attributes");
            }
            let mut raw = original;
            // SAFETY: `raw` is a valid termios.
            unsafe { libc::cfmakeraw(&mut raw) };
            // SAFETY: `fd` is stdin; `raw` is a valid termios.
            if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
                anyhow::bail!("failed to set raw mode");
            }
            Ok(Self { original })
        }
    }

    impl Drop for RawTerminal {
        fn drop(&mut self) {
            use std::os::unix::io::AsRawFd;
            let fd = std::io::stdin().as_raw_fd();
            // Restore at once, then discard unread typed-ahead input. Not
            // TCSAFLUSH/TCSADRAIN: those also wait for pending output to drain,
            // which never happens if the terminal has stopped reading. The input
            // flush must come after: re-entering canonical mode makes the BSD
            // tty driver set PENDIN, and flushing input is what clears it.
            // SAFETY: `fd` is stdin; `original` came from tcgetattr on this fd;
            // tcflush takes a queue selector.
            unsafe {
                libc::tcsetattr(fd, libc::TCSANOW, &self.original);
                libc::tcflush(fd, libc::TCIFLUSH);
            }
        }
    }

    /// Messages waiting for the sender thread; controls go first.
    #[derive(Default)]
    struct Outbox {
        state: Mutex<OutboxState>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct OutboxState {
        controls: VecDeque<Frame>,
        data: VecDeque<Vec<u8>>,
        data_bytes: usize,
        sending_control: bool,
        closed: bool,
    }

    impl Outbox {
        fn lock(&self) -> MutexGuard<'_, OutboxState> {
            self.state.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn push_control(&self, frame: Frame) {
            self.lock().controls.push_back(frame);
            self.changed.notify_all();
        }

        /// Queue keystrokes; `false` if dropped because the backlog is full.
        fn push_data(&self, bytes: Vec<u8>) -> bool {
            let mut state = self.lock();
            if state.data_bytes + bytes.len() > OUTBOX_DATA_LIMIT {
                return false;
            }
            state.data_bytes += bytes.len();
            state.data.push_back(bytes);
            drop(state);
            self.changed.notify_all();
            true
        }

        /// The next message to send, controls first; `None` once closed.
        fn next(&self) -> Option<Frame> {
            let mut state = self.lock();
            loop {
                if state.closed {
                    return None;
                }
                if let Some(control) = state.controls.pop_front() {
                    state.sending_control = true;
                    return Some(control);
                }
                if let Some(data) = state.data.pop_front() {
                    state.data_bytes -= data.len();
                    return Some(Frame::Data(data));
                }
                state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        }

        fn sent(&self) {
            self.lock().sending_control = false;
            self.changed.notify_all();
        }

        /// Wait up to `grace` for queued controls to be written.
        fn wait_controls_sent(&self, grace: Duration) {
            let deadline = Instant::now() + grace;
            let mut state = self.lock();
            while !state.closed && (!state.controls.is_empty() || state.sending_control) {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    return;
                };
                state = self
                    .changed
                    .wait_timeout(state, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }

        fn close(&self) {
            self.lock().closed = true;
            self.changed.notify_all();
        }
    }

    fn spawn_sender(mut stream: UnixStream, outbox: Arc<Outbox>, closed: Arc<AtomicBool>) {
        std::thread::spawn(move || {
            while let Some(frame) = outbox.next() {
                let written = attach_proto::write_frame(&mut stream, &frame);
                outbox.sent();
                if written.is_err() {
                    closed.store(true, Ordering::SeqCst);
                    outbox.close();
                    return;
                }
            }
        });
    }

    fn spawn_reader(mut stream: UnixStream, closed: Arc<AtomicBool>, retired: Arc<AtomicBool>) {
        std::thread::spawn(move || {
            let mut stdout = std::io::stdout().lock();
            loop {
                match attach_proto::read_frame(&mut stream) {
                    Ok(Frame::Data(payload)) => {
                        if stdout.write_all(&payload).is_err() || stdout.flush().is_err() {
                            break;
                        }
                    }
                    Ok(Frame::Retired(_)) => retired.store(true, Ordering::SeqCst),
                    Ok(_) | Err(FrameError::Malformed(_) | FrameError::Unknown(_)) => {}
                    Err(FrameError::Io(_)) => break,
                }
            }
            closed.store(true, Ordering::SeqCst);
        });
    }

    fn stdin_readable(timeout: Duration) -> bool {
        use rustix::event::{PollFd, PollFlags, poll};
        let stdin = std::io::stdin();
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        let timespec = poll_timespec(timeout);
        matches!(poll(&mut fds, Some(&timespec)), Ok(n) if n > 0)
    }

    /// The poll timeout for `timeout`, whole seconds included (saturating for a
    /// duration too long to represent).
    fn poll_timespec(timeout: Duration) -> rustix::event::Timespec {
        rustix::event::Timespec::try_from(timeout).unwrap_or(rustix::event::Timespec {
            tv_sec: i64::MAX,
            tv_nsec: 999_999_999,
        })
    }

    /// The terminal's size; `None` if unknown or if either dimension is zero,
    /// which the session could not apply.
    fn terminal_size() -> Option<tendr::recording::Geometry> {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdout().as_raw_fd();
        // SAFETY: winsize is plain old data; TIOCGWINSZ fills it on success.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is stdout; TIOCGWINSZ takes a winsize out-pointer.
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 {
            tendr::recording::Geometry::new(ws.ws_row, ws.ws_col)
        } else {
            None
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn poll_timespec_carries_whole_seconds() {
            let ts = poll_timespec(Duration::from_millis(1500));
            assert_eq!((ts.tv_sec, ts.tv_nsec), (1, 500_000_000));
            let ts = poll_timespec(Duration::from_millis(100));
            assert_eq!((ts.tv_sec, ts.tv_nsec), (0, 100_000_000));
        }
    }
}
