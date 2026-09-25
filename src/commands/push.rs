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
/// Succeeds only if the sidecar reports that every byte it received was written
/// to the terminal. A push refused because another client holds the terminal,
/// or revoked by a takeover part-way, fails with how many bytes were written.
#[cfg(unix)]
fn push_over_attach_socket(session_dir: &std::path::Path) -> anyhow::Result<()> {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use tendr::attach_proto::{self, INPUT_CLOSED, INPUT_REVOKED, INPUT_WRITTEN};

    let sock_path = attach_proto::read_sock_path(session_dir)
        .ok_or_else(|| anyhow::anyhow!("attach socket not found"))?;
    let mut stream = UnixStream::connect(&sock_path)?;
    tendr::attach_socket::verify_peer(&stream)
        .map_err(|e| anyhow::anyhow!("refusing attach socket {}: {e}", sock_path.display()))?;

    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    attach_proto::write_msg(
        &mut stream,
        attach_proto::MSG_HELLO,
        &[attach_proto::PROTOCOL_VERSION, attach_proto::MODE_PUSH],
    )?;
    match attach_proto::read_msg(&mut stream) {
        Ok((attach_proto::MSG_ACCEPTED, _)) => {}
        Ok((attach_proto::MSG_REJECTED, reason)) => {
            anyhow::bail!("push refused: {}", String::from_utf8_lossy(&reason));
        }
        Ok((other, _)) => anyhow::bail!("unexpected push reply {other:#04x}"),
        Err(e) => anyhow::bail!(
            "the session did not accept the push ({e}); it may predate acknowledged push"
        ),
    }
    // A push waits as long as the terminal applies backpressure.
    stream.set_read_timeout(None)?;

    let mut stdin = std::io::stdin().lock();
    let mut buf = vec![0u8; attach_proto::MAX_FRAME_PAYLOAD];
    let mut sent: u64 = 0;
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if attach_proto::write_msg(&mut stream, attach_proto::MSG_DATA, &buf[..n]).is_err() {
            // The sidecar stopped reading: its outcome explains why.
            break;
        }
        sent += n as u64;
    }
    let _ = attach_proto::write_msg(&mut stream, attach_proto::MSG_DETACH, &[]);

    loop {
        match attach_proto::read_msg(&mut stream) {
            Ok((attach_proto::MSG_INPUT_DONE, payload)) => {
                let Some((status, accepted, _received)) = attach_proto::parse_input_done(&payload)
                else {
                    anyhow::bail!("malformed push outcome from the session");
                };
                return match status {
                    INPUT_WRITTEN => Ok(()),
                    INPUT_REVOKED => anyhow::bail!(
                        "push revoked after {accepted} of {sent} bytes: another client took control of the terminal"
                    ),
                    INPUT_CLOSED => anyhow::bail!(
                        "push stopped after {accepted} of {sent} bytes: the session stopped accepting input"
                    ),
                    other => anyhow::bail!("unknown push outcome {other}"),
                };
            }
            Ok(_) => {}
            Err(e) => anyhow::bail!(
                "the session closed the push without reporting an outcome after {sent} bytes ({e})"
            ),
        }
    }
}
