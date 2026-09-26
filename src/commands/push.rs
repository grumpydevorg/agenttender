use tendr::model::ids::{Namespace, SessionName};
use tendr::model::pty::PtyControl;
use tendr::model::spec::StdinMode;
use tendr::model::state::RunStatus;
use tendr::platform::{Current, Platform};
use tendr::session::{self, SessionRoot};

pub fn cmd_push(name: &str, namespace: &Namespace) -> anyhow::Result<()> {
    let session_name = SessionName::new(name)?;
    let root = SessionRoot::default_path()?;

    let session = session::open(&root, namespace, &session_name)?
        .ok_or_else(|| anyhow::anyhow!("session not found: {name}"))?;

    let meta = session::read_meta(&session)?;

    // Push requires Running state explicitly
    if !matches!(meta.status(), RunStatus::Running { .. }) {
        anyhow::bail!("session is not running");
    }

    // Reject push while a human is attached to a PTY session
    if let Some(pty) = meta.pty() {
        if pty.control == PtyControl::HumanControl {
            anyhow::bail!("session is under human control");
        }
    }

    if meta.launch_spec().stdin_mode != StdinMode::Pipe {
        anyhow::bail!("session was not started with --stdin");
    }

    // PTY sessions take pushes over the attach socket, which reports exactly
    // what reached the terminal; pipe sessions keep the stdin FIFO.
    #[cfg(unix)]
    if meta.pty().is_some() {
        return push_over_attach_socket(session.path());
    }

    let mut fifo = loop {
        match Current::open_stdin_writer(session.path()) {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                // No reader connected -- check if session is still running
                let current = session::read_meta(&session)?;
                if !matches!(current.status(), RunStatus::Running { .. }) {
                    anyhow::bail!("session exited before push could connect");
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to open stdin pipe: {e}"));
            }
        }
    };

    let mut stdin = std::io::stdin().lock();
    std::io::copy(&mut stdin, &mut fifo)?;

    Ok(())
}

/// Push stdin to a PTY session as an acknowledged agent input stream.
///
/// Succeeds only if every byte of stdin was written to the terminal
/// ([`push_outcome`]). A push refused because another client holds the
/// terminal, or revoked by a takeover part-way, fails with how many bytes were
/// written.
#[cfg(unix)]
fn push_over_attach_socket(session_dir: &std::path::Path) -> anyhow::Result<()> {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use tendr::attach_proto::{self, Frame, FrameError, Mode, read_frame, write_frame};

    let sock_path = attach_proto::read_sock_path(session_dir)
        .ok_or_else(|| anyhow::anyhow!("attach socket not found"))?;
    let mut stream = UnixStream::connect(&sock_path)?;
    tendr::attach_socket::verify_peer(&stream)
        .map_err(|e| anyhow::anyhow!("refusing attach socket {}: {e}", sock_path.display()))?;

    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    write_frame(&mut stream, &Frame::Hello(Mode::Push))?;
    match read_frame(&mut stream) {
        Ok(Frame::Accepted(_)) => {}
        Ok(Frame::Rejected(reason)) => anyhow::bail!("push refused: {reason}"),
        Ok(other) => anyhow::bail!("unexpected push reply {other:?}"),
        Err(FrameError::Unknown(msg_type)) => anyhow::bail!(
            "the session replied with attach message type {msg_type}, which this tendr does not know; it may be newer"
        ),
        Err(e) => anyhow::bail!(
            "the session did not accept the push ({e}); it may predate acknowledged push"
        ),
    }
    // A push waits as long as the terminal applies backpressure.
    stream.set_read_timeout(None)?;

    let mut stdin = std::io::stdin().lock();
    let mut buf = vec![0u8; attach_proto::MAX_FRAME_PAYLOAD];
    let mut sent: u64 = 0;
    let mut all_sent = false;
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => {
                all_sent = true;
                break;
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if write_frame(&mut stream, &Frame::Data(buf[..n].to_vec())).is_err() {
            // The sidecar stopped reading: its outcome explains why.
            break;
        }
        sent += n as u64;
    }
    let _ = write_frame(&mut stream, &Frame::Detach);

    loop {
        match read_frame(&mut stream) {
            Ok(Frame::InputDone(outcome)) => return push_outcome(outcome, sent, all_sent),
            Err(FrameError::Malformed(e)) => {
                anyhow::bail!("malformed push outcome from the session: {e}")
            }
            Ok(_) | Err(FrameError::Unknown(_)) => {}
            Err(FrameError::Io(e)) => anyhow::bail!(
                "the session closed the push without reporting an outcome after {sent} bytes ({e})"
            ),
        }
    }
}

/// The push's result from the session's `outcome`, given that `sent` bytes
/// were sent and `all_sent` is whether stdin was sent to its end.
///
/// Success means every byte was written. A push stopped after its last byte was
/// written (a takeover while its end marker was still on its way) wrote them
/// all, so it succeeds whatever the outcome (PR #68 review, finding 4).
#[cfg(unix)]
fn push_outcome(
    outcome: tendr::attach_proto::PushOutcome,
    sent: u64,
    all_sent: bool,
) -> anyhow::Result<()> {
    use tendr::attach_proto::PushOutcome;

    let every_byte_written = |accepted: u64| all_sent && accepted == sent;
    match outcome {
        PushOutcome::Written { .. } => Ok(()),
        PushOutcome::Revoked(p) | PushOutcome::Closed(p) if every_byte_written(p.accepted) => {
            Ok(())
        }
        PushOutcome::Revoked(p) => anyhow::bail!(
            "push revoked after {} of {sent} bytes: another client took control of the terminal",
            p.accepted
        ),
        PushOutcome::Closed(p) => anyhow::bail!(
            "push stopped after {} of {sent} bytes: the session stopped accepting input",
            p.accepted
        ),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::push_outcome;
    use tendr::attach_proto::{Progress, PushOutcome};

    fn stopped(accepted: u64, received: u64) -> Progress {
        Progress {
            accepted,
            unwritten: received - accepted,
        }
    }

    /// A takeover that lands after the last byte was written, while the end
    /// marker is still on its way, ends the push `revoked` with nothing left
    /// unwritten. The push succeeded (PR #68 review, finding 4).
    #[test]
    fn a_push_whose_every_byte_was_written_succeeds_whatever_the_outcome() {
        assert!(push_outcome(PushOutcome::Written { bytes: 10 }, 10, true).is_ok());
        assert!(push_outcome(PushOutcome::Revoked(stopped(10, 10)), 10, true).is_ok());
        assert!(push_outcome(PushOutcome::Closed(stopped(10, 10)), 10, true).is_ok());
    }

    #[test]
    fn a_push_with_bytes_unwritten_fails() {
        assert!(push_outcome(PushOutcome::Revoked(stopped(4, 10)), 10, true).is_err());
        assert!(push_outcome(PushOutcome::Closed(stopped(4, 10)), 10, true).is_err());
        // Stdin was not sent to its end: bytes the session never saw are unwritten.
        assert!(push_outcome(PushOutcome::Revoked(stopped(10, 10)), 10, false).is_err());
    }
}
