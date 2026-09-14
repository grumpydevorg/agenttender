#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

use tender::attach_proto;
use tender::model::ids::{Namespace, SessionName};
use tender::model::pty::PtyControl;
use tender::model::state::RunStatus;
use tender::session::{self, SessionRoot};

pub fn cmd_attach(name: &str, namespace: &Namespace, takeover: bool) -> anyhow::Result<()> {
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

    let sock_path = attach_proto::read_sock_path(session.path())
        .ok_or_else(|| anyhow::anyhow!("attach socket not found"))?;

    #[cfg(unix)]
    {
        // Fail before any side effect — a takeover included — if there is no
        // terminal to drive.
        if !rustix::termios::isatty(std::io::stdin()) {
            anyhow::bail!("attach requires an interactive terminal on stdin");
        }

        let mut stream = UnixStream::connect(&sock_path)?;
        handshake(&mut stream, takeover)?;
        relay(stream)
    }

    #[cfg(not(unix))]
    {
        let _ = (sock_path, takeover);
        anyhow::bail!("attach is only supported on Unix");
    }
}

/// Send the v1 hello and require the sidecar's acceptance.
#[cfg(unix)]
fn handshake(stream: &mut UnixStream, takeover: bool) -> anyhow::Result<()> {
    let mode = if takeover {
        attach_proto::MODE_TAKEOVER
    } else {
        attach_proto::MODE_ATTACH
    };
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    attach_proto::write_msg(
        stream,
        attach_proto::MSG_HELLO,
        &[attach_proto::PROTOCOL_VERSION, mode],
    )?;
    match attach_proto::read_msg(stream) {
        Ok((attach_proto::MSG_ACCEPTED, _)) => {}
        Ok((attach_proto::MSG_REJECTED, reason)) => {
            anyhow::bail!("attach rejected: {}", String::from_utf8_lossy(&reason));
        }
        Ok((other, _)) => anyhow::bail!("unexpected attach reply {other:#04x}"),
        Err(e) => anyhow::bail!(
            "the session did not complete the attach handshake ({e}); it may predate attach protocol v1"
        ),
    }
    stream.set_read_timeout(None)?;
    Ok(())
}

/// Relay the terminal until detach, disconnect, or retirement.
#[cfg(unix)]
fn relay(stream: UnixStream) -> anyhow::Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut read_stream = stream.try_clone()?;
    let mut write_stream = stream;
    let closed = Arc::new(AtomicBool::new(false));
    let retired = Arc::new(AtomicBool::new(false));

    let orig = enter_raw_mode()?;

    if let Some((rows, cols)) = terminal_size() {
        let payload = attach_proto::resize_payload(rows, cols);
        let _ = attach_proto::write_msg(&mut write_stream, attach_proto::MSG_RESIZE, &payload);
    }

    // Reader thread: socket -> stdout, until the sidecar closes the connection.
    let reader_handle = {
        let closed = Arc::clone(&closed);
        let retired = Arc::clone(&retired);
        std::thread::spawn(move || {
            let mut stdout = std::io::stdout().lock();
            loop {
                match attach_proto::read_msg(&mut read_stream) {
                    Ok((attach_proto::MSG_DATA, payload)) => {
                        if stdout.write_all(&payload).is_err() || stdout.flush().is_err() {
                            break;
                        }
                    }
                    Ok((attach_proto::MSG_RETIRED, _)) => retired.store(true, Ordering::SeqCst),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            closed.store(true, Ordering::SeqCst);
        })
    };

    // Main thread: stdin -> socket. Poll so a closed connection ends the relay
    // without waiting for the next keystroke.
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 1024];
    while !closed.load(Ordering::SeqCst) {
        if !stdin_readable(std::time::Duration::from_millis(100)) {
            continue;
        }
        let n = match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if attach_proto::write_msg(&mut write_stream, attach_proto::MSG_DATA, &buf[..n]).is_err() {
            break;
        }
    }

    let _ = attach_proto::write_msg(&mut write_stream, attach_proto::MSG_DETACH, &[]);
    let _ = write_stream.shutdown(std::net::Shutdown::Both);
    restore_terminal(&orig);
    let _ = reader_handle.join();
    if retired.load(Ordering::SeqCst) {
        eprintln!("tender: another client took over this session");
    }
    Ok(())
}

#[cfg(unix)]
fn stdin_readable(timeout: std::time::Duration) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let stdin = std::io::stdin();
    let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
    let timespec = Timespec {
        tv_sec: 0,
        tv_nsec: i64::from(timeout.subsec_nanos()),
    };
    matches!(poll(&mut fds, Some(&timespec)), Ok(n) if n > 0)
}

#[cfg(unix)]
fn enter_raw_mode() -> anyhow::Result<libc::termios> {
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    let mut orig: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
        anyhow::bail!("failed to get terminal attributes");
    }
    let mut raw = orig;
    unsafe {
        libc::cfmakeraw(&mut raw);
    }
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
        anyhow::bail!("failed to set raw mode");
    }
    Ok(orig)
}

#[cfg(unix)]
fn restore_terminal(orig: &libc::termios) {
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    unsafe {
        libc::tcsetattr(fd, libc::TCSAFLUSH, orig);
    }
}

#[cfg(unix)]
fn terminal_size() -> Option<(u16, u16)> {
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stdout().as_raw_fd();
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}
