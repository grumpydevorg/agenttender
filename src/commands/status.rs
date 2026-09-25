use tendr::model::ids::{Namespace, ProcessIdentity, SessionName};
use tendr::platform::{Current, Platform, ProcessStatus};
use tendr::session::{self, SessionError, SessionRoot};

pub fn cmd_status(name: &str, namespace: &Namespace) -> anyhow::Result<()> {
    let session_name = SessionName::new(name)?;
    let root = SessionRoot::default_path()?;

    // Try normal open first
    let session = match session::open(&root, namespace, &session_name) {
        Ok(Some(s)) => s,
        Ok(None) => anyhow::bail!("session not found: {name}"),
        Err(SessionError::Corrupt { .. }) => {
            // Check for orphan dir (child_pid but no meta.json)
            let orphan_dir = root
                .path()
                .join(namespace.as_str())
                .join(session_name.as_str());
            if orphan_dir.exists() {
                cleanup_orphan_dir(&orphan_dir);
                anyhow::bail!("session {name} was orphaned (cleaned up)");
            }
            anyhow::bail!("session not found: {name}");
        }
        Err(e) => return Err(e.into()),
    };

    let mut meta = session::read_meta(&session)?;

    // Reconciliation: non-terminal + lock not held -> sidecar gone.
    // Heals from the event log when the sidecar's own terminal event
    // exists; infers SidecarLost otherwise (spec §3.6).
    tendr::reconcile::reconcile_sidecar_gone(&session, &mut meta)?;

    let json = serde_json::to_string_pretty(&meta)?;
    println!("{json}");
    Ok(())
}

/// Clean up an orphaned session dir that has child_pid but no meta.json.
/// The child_pid breadcrumb contains a JSON-serialized ProcessIdentity,
/// which lets us verify the process against PID reuse before killing.
/// Falls back to skip-kill for old bare-PID format breadcrumbs.
pub(crate) fn cleanup_orphan_dir(dir: &std::path::Path) {
    let child_pid_path = dir.join("child_pid");
    if let Ok(content) = std::fs::read_to_string(&child_pid_path) {
        // Try JSON ProcessIdentity (new format)
        if let Ok(identity) = serde_json::from_str::<ProcessIdentity>(&content) {
            match Current::process_status(&identity) {
                ProcessStatus::AliveVerified | ProcessStatus::Inaccessible => {
                    // Identity verified (or can't verify but process exists) -- kill it
                    let _ = Current::kill_orphan(&identity, true);
                }
                // Missing, IdentityMismatch, OsError -- don't kill
                _ => {}
            }
        }
        // Old bare-PID format: can't verify identity, skip kill (backwards compat)
    }
    let _ = std::fs::remove_dir_all(dir);
}
