#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(unix)]
use tender::attach_proto;
use tender::model::ids::{Namespace, SessionName};
use tender::model::pty::PtyControl;
use tender::model::state::RunStatus;
use tender::session::{self, SessionRoot};

pub fn cmd_attach(name: &str, namespace: &Namespace) -> anyhow::Result<()> {
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

    if pty.control == PtyControl::HumanControl {
        anyhow::bail!("session is already under human control");
    }

    #[cfg(not(unix))]
    anyhow::bail!("attach is only supported on Unix");

    #[cfg(unix)]
    {
        let sock_path = attach_proto::read_sock_path(session.path())
            .ok_or_else(|| anyhow::anyhow!("attach socket not found"))?;
        let stream = UnixStream::connect(&sock_path)?;
        let mut read_stream = stream.try_clone()?;
        let mut write_stream = stream;

        // Put terminal in raw mode
        let orig = enter_raw_mode()?;

        // Send initial resize
        if let Some((rows, cols)) = terminal_size() {
            let payload = attach_proto::resize_payload(rows, cols);
            let _ = attach_proto::write_msg(&mut write_stream, attach_proto::MSG_RESIZE, &payload);
        }

        // Spawn reader thread: socket -> stdout
        let reader_handle = std::thread::spawn(move || {
            let mut stdout = std::io::stdout().lock();
            loop {
                match attach_proto::read_msg(&mut read_stream) {
                    Ok((attach_proto::MSG_DATA, payload)) => {
                        if stdout.write_all(&payload).is_err() || stdout.flush().is_err() {
                            break;
                        }
                    }
                    Err(_) => break, // Socket closed
                    _ => {}          // Ignore unknown messages
                }
            }
        });

        // Main thread: stdin -> socket
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 1024];
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if attach_proto::write_msg(&mut write_stream, attach_proto::MSG_DATA, &buf[..n])
                .is_err()
            {
                break;
            }
        }

        // Send detach and restore terminal
        let _ = attach_proto::write_msg(&mut write_stream, attach_proto::MSG_DETACH, &[]);
        restore_terminal(&orig);
        let _ = reader_handle.join();
        Ok(())
    }
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
