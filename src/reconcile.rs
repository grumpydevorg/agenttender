//! CLI-side reconciliation of sessions whose sidecar is gone — the shared
//! path behind `wait`/`status`/`run` (spec §3.6 of event-protocol.md).
//!
//! Before inferring `run.sidecar_lost`, read the event-log tail: if the
//! sidecar's own terminal event exists (it died in the WAL crash window
//! between the event append and the meta write), heal meta from that record
//! instead of inferring loss.

use std::num::NonZeroI32;
use std::path::Path;

use crate::events::{self, EventDraft, EventWriter, read_session_events};
use crate::model::dep_fail::DepFailReason;
use crate::model::event::{Event, Uuid7};
use crate::model::ids::{EpochTimestamp, Namespace, Source};
use crate::model::meta::Meta;
use crate::model::state::ExitReason;
use crate::model::transition::HealedTerminal;
use crate::session::{self, SessionDir};

/// What reconciliation did to meta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconciled {
    /// Meta was already terminal, or a live sidecar holds the lock.
    Untouched,
    /// Meta healed from the sidecar's own terminal event
    /// (Direct provenance, EventLogTerminal evidence).
    Healed,
    /// No terminal event found: SidecarLost inferred, and the inferred
    /// `run.sidecar_lost` event appended to the log.
    Inferred,
}

/// Reconcile a session whose meta is non-terminal while nothing holds its
/// lock. Writes meta atomically when it changes it; a no-op otherwise.
///
/// # Errors
/// Returns lock-probe, meta-write, or transition errors. Event-log reads and
/// the inferred-event append are best-effort — they never fail reconciliation.
pub fn reconcile_sidecar_gone(session: &SessionDir, meta: &mut Meta) -> anyhow::Result<Reconciled> {
    if meta.status().is_terminal() || session::is_locked(session)? {
        return Ok(Reconciled::Untouched);
    }

    if let Some(event) = find_sidecar_terminal_event(session.path(), meta) {
        if let Some(healed) = healed_terminal_of(&event) {
            let ended_at = EpochTimestamp::from_secs(event.ts.epoch_secs());
            if meta.heal_terminal_from_event(healed, ended_at).is_ok() {
                session::write_meta_atomic(session, meta)?;
                return Ok(Reconciled::Healed);
            }
        }
    }

    meta.reconcile_sidecar_lost(EpochTimestamp::now())?;
    // WAL discipline holds for the inferred record too: event before meta.
    append_sidecar_lost_event(session, meta);
    session::write_meta_atomic(session, meta)?;
    Ok(Reconciled::Inferred)
}

/// The sidecar's own most recent terminal event for meta's run, if any.
/// Best-effort: unreadable logs mean "no evidence", never an error.
fn find_sidecar_terminal_event(session_dir: &Path, meta: &Meta) -> Option<Event> {
    let outcome = read_session_events(session_dir).ok()?;
    let run_id = meta.run_id();
    let sidecar_writer = Uuid7::from(run_id);
    outcome.events.into_iter().rev().find(|event| {
        event.run_id == run_id
            && event.writer == sidecar_writer
            && event.source.as_str() == "tender.sidecar"
            && matches!(
                event.kind.as_str(),
                "run.exited"
                    | "run.killed"
                    | "run.timed_out"
                    | "run.spawn_failed"
                    | "run.dependency_failed"
            )
    })
}

/// Parse a lifecycle event's `data` back into a terminal outcome.
/// Returns `None` on any shape surprise — the caller then infers loss.
fn healed_terminal_of(event: &Event) -> Option<HealedTerminal> {
    let data = event.data.as_ref()?;
    match data["status"].as_str()? {
        "Exited" => {
            let how = match data["reason"].as_str()? {
                "ExitedOk" => ExitReason::ExitedOk,
                "ExitedError" => ExitReason::ExitedError {
                    code: NonZeroI32::new(i32::try_from(data["exit_code"].as_i64()?).ok()?)?,
                },
                "Killed" => ExitReason::Killed,
                "KilledForced" => ExitReason::KilledForced,
                "TimedOut" => ExitReason::TimedOut,
                _ => return None,
            };
            Some(HealedTerminal::Exited(how))
        }
        "SpawnFailed" => Some(HealedTerminal::SpawnFailed),
        "DependencyFailed" => {
            let reason = match data["reason"].as_str()? {
                "Failed" => DepFailReason::Failed,
                "TimedOut" => DepFailReason::TimedOut,
                "Killed" => DepFailReason::Killed,
                "KilledForced" => DepFailReason::KilledForced,
                _ => return None,
            };
            Some(HealedTerminal::DependencyFailed(reason))
        }
        _ => None,
    }
}

/// Append the inferred `run.sidecar_lost` event (`data.provenance:
/// "inferred"`, source `tender.cli`, fresh CLI writer identity). Best-effort:
/// reconciliation must not fail because the history log is unwritable.
fn append_sidecar_lost_event(session: &SessionDir, meta: &Meta) {
    let Some(namespace) = namespace_of(session) else {
        return;
    };
    let Ok(source) = Source::trusted("tender.cli") else {
        return;
    };
    let draft = EventDraft {
        id: None,
        kind: events::lifecycle_kind(meta.status()),
        namespace,
        session: meta.session().clone(),
        run_id: meta.run_id(),
        generation: Some(meta.generation().as_u64()),
        source,
        block_id: None,
        parent_id: None,
        data: Some(events::lifecycle_data(
            meta.status(),
            "inferred",
            meta.launch_spec().boundary.as_ref(),
        )),
        preview: None,
    };
    let mut writer = EventWriter::new(session.path());
    let _ = writer.append(draft, true);
}

/// Namespace from the session dir's structure: `root/<namespace>/<session>/`.
fn namespace_of(session: &SessionDir) -> Option<Namespace> {
    let name = session.path().parent()?.file_name()?.to_str()?;
    Namespace::new(name).ok()
}
