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
