use std::path::PathBuf;

use tendr::platform::{Current, Platform};

pub fn cmd_sidecar(session_dir: PathBuf) -> anyhow::Result<()> {
    // On Windows, allocate a hidden console so children can receive
    // GenerateConsoleCtrlEvent for graceful stop.
    #[cfg(windows)]
    tendr::platform::windows::prepare_sidecar_console();

    let ready_writer = Current::ready_writer_from_env()?;
    tendr::sidecar::run(session_dir, ready_writer)
}
