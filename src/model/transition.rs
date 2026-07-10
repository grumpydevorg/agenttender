use thiserror::Error;

use super::dep_fail::DepFailReason;
use super::ids::{EpochTimestamp, ProcessIdentity};
use super::meta::Meta;
use super::provenance::{Evidence, TransitionProvenance};
use super::state::{ExitReason, RunStatus};

#[derive(Debug, Error)]
pub enum TransitionError {
    #[error("cannot transition from {from} — already terminal")]
    AlreadyTerminal { from: &'static str },
    #[error("illegal transition from {from} to {to}")]
    Illegal {
        from: &'static str,
        to: &'static str,
    },
}

fn status_name(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Starting => "Starting",
        RunStatus::Running { .. } => "Running",
        RunStatus::SpawnFailed { .. } => "SpawnFailed",
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk => "ExitedOk",
            ExitReason::ExitedError { .. } => "ExitedError",
            ExitReason::Killed => "Killed",
            ExitReason::KilledForced => "KilledForced",
            ExitReason::TimedOut => "TimedOut",
        },
        RunStatus::SidecarLost { .. } => "SidecarLost",
        RunStatus::DependencyFailed { .. } => "DependencyFailed",
    }
}

impl Meta {
    /// Transition Starting → Running. Requires child identity.
    pub fn transition_running(&mut self, child: ProcessIdentity) -> Result<(), TransitionError> {
        match self.status() {
            RunStatus::Starting => {
                *self.status_mut() = RunStatus::Running { child };
                self.set_transition_provenance(TransitionProvenance::direct(&[
                    Evidence::SidecarWrite,
                    Evidence::ChildSpawned,
                ]));
                Ok(())
            }
            RunStatus::Running { .. } => Err(TransitionError::Illegal {
                from: "Running",
                to: "Running",
            }),
            _ => Err(TransitionError::AlreadyTerminal {
                from: status_name(self.status()),
            }),
        }
    }

    /// Transition Starting → SpawnFailed. Child never started.
    /// Only valid from Starting — cannot reach SpawnFailed from Running.
    pub fn transition_spawn_failed(
        &mut self,
        ended_at: EpochTimestamp,
    ) -> Result<(), TransitionError> {
        match self.status() {
            RunStatus::Starting => {
                *self.status_mut() = RunStatus::SpawnFailed { ended_at };
                self.set_transition_provenance(TransitionProvenance::direct(&[
                    Evidence::SidecarWrite,
                    Evidence::SpawnFailedSyscall,
                ]));
                Ok(())
            }
            RunStatus::Running { .. } => Err(TransitionError::Illegal {
                from: "Running",
                to: "SpawnFailed",
            }),
            _ => Err(TransitionError::AlreadyTerminal {
                from: status_name(self.status()),
            }),
        }
    }

    /// Transition Running → Exited. Only valid from Running.
    /// ExitReason cannot include SpawnFailed — that's a separate type.
    /// Child identity is carried from Running into the Exited state.
    pub fn transition_exited(
        &mut self,
        how: ExitReason,
        ended_at: EpochTimestamp,
    ) -> Result<(), TransitionError> {
        match self.status() {
            RunStatus::Running { child } => {
                let child = *child;
                *self.status_mut() = RunStatus::Exited {
                    child,
                    how,
                    ended_at,
                };
                self.set_transition_provenance(TransitionProvenance::direct(&[
                    Evidence::SidecarWrite,
                    Evidence::ChildExitObserved,
                ]));
                Ok(())
            }
            RunStatus::Starting => Err(TransitionError::Illegal {
                from: "Starting",
                to: "Exited",
            }),
            _ => Err(TransitionError::AlreadyTerminal {
                from: status_name(self.status()),
            }),
        }
    }

    /// Transition Starting → DependencyFailed.
    pub fn transition_dependency_failed(
        &mut self,
        ended_at: EpochTimestamp,
        reason: DepFailReason,
    ) -> Result<(), TransitionError> {
        match self.status() {
            RunStatus::Starting => {
                *self.status_mut() = RunStatus::DependencyFailed { ended_at, reason };
                self.set_transition_provenance(TransitionProvenance::direct(&[
                    Evidence::SidecarWrite,
                    Evidence::DependencyFailed,
                ]));
                Ok(())
            }
            RunStatus::Running { .. } => Err(TransitionError::Illegal {
                from: "Running",
                to: "DependencyFailed",
            }),
            _ => Err(TransitionError::AlreadyTerminal {
                from: status_name(self.status()),
            }),
        }
    }

    /// Reconciliation: mark as SidecarLost. The ONLY case where
    /// something other than the sidecar writes lifecycle state.
    /// Valid from Starting (no child) or Running (with child).
    pub fn reconcile_sidecar_lost(
        &mut self,
        ended_at: EpochTimestamp,
    ) -> Result<(), TransitionError> {
        match self.status() {
            RunStatus::Starting => {
                *self.status_mut() = RunStatus::SidecarLost {
                    child: None,
                    ended_at,
                };
                self.set_transition_provenance(TransitionProvenance::inferred(&[
                    Evidence::LockReleased,
                    Evidence::NonTerminalMeta,
                ]));
                Ok(())
            }
            RunStatus::Running { child } => {
                let child = *child;
                *self.status_mut() = RunStatus::SidecarLost {
                    child: Some(child),
                    ended_at,
                };
                self.set_transition_provenance(TransitionProvenance::inferred(&[
                    Evidence::LockReleased,
                    Evidence::NonTerminalMeta,
                ]));
                Ok(())
            }
            _ => Err(TransitionError::AlreadyTerminal {
                from: status_name(self.status()),
            }),
        }
    }

    /// Reconciliation healing: apply a terminal state recorded by the
    /// sidecar's own event-log record (spec §3.6). The observation was the
    /// sidecar's — the CLI only replays it into meta — so provenance is
    /// Direct with EventLogTerminal evidence, not Inferred.
    pub fn heal_terminal_from_event(
        &mut self,
        healed: HealedTerminal,
        ended_at: EpochTimestamp,
    ) -> Result<(), TransitionError> {
        match (self.status(), healed) {
            (RunStatus::Running { child }, HealedTerminal::Exited(how)) => {
                let child = *child;
                *self.status_mut() = RunStatus::Exited {
                    child,
                    how,
                    ended_at,
                };
            }
            (RunStatus::Starting, HealedTerminal::SpawnFailed) => {
                *self.status_mut() = RunStatus::SpawnFailed { ended_at };
            }
            (RunStatus::Starting, HealedTerminal::DependencyFailed(reason)) => {
                *self.status_mut() = RunStatus::DependencyFailed { ended_at, reason };
            }
            (status, _) if status.is_terminal() => {
                return Err(TransitionError::AlreadyTerminal {
                    from: status_name(status),
                });
            }
            (status, _) => {
                // Shape mismatch (e.g. meta Starting but event says Exited):
                // the caller falls back to inferring SidecarLost.
                return Err(TransitionError::Illegal {
                    from: status_name(status),
                    to: "healed terminal",
                });
            }
        }
        self.set_transition_provenance(TransitionProvenance::direct(&[Evidence::EventLogTerminal]));
        Ok(())
    }
}

/// A terminal outcome parsed from the sidecar's own event-log record, used
/// to heal meta when the sidecar died between the event append and the meta
/// write (the WAL crash window, spec §3.6).
#[derive(Debug, Clone)]
pub enum HealedTerminal {
    Exited(ExitReason),
    SpawnFailed,
    DependencyFailed(DepFailReason),
}

#[cfg(test)]
mod provenance_tests {
    use super::*;
    use crate::model::ids::{EpochTimestamp, Generation, ProcessIdentity, RunId, SessionName};
    use crate::model::provenance::Evidence;
    use crate::model::spec::LaunchSpec;

    fn fresh_meta() -> Meta {
        Meta::new_starting(
            SessionName::new("t").unwrap(),
            RunId::new(),
            Generation::first(),
            LaunchSpec::new(vec!["bash".to_owned()]).unwrap(),
            ProcessIdentity {
                pid: std::num::NonZero::new(1).unwrap(),
                start_time_ns: 0,
            },
            EpochTimestamp::from_secs(0),
        )
    }

    fn child() -> ProcessIdentity {
        ProcessIdentity {
            pid: std::num::NonZero::new(2).unwrap(),
            start_time_ns: 0,
        }
    }

    #[test]
    fn running_is_direct_with_child_spawned() {
        let mut m = fresh_meta();
        m.transition_running(child()).unwrap();
        let p = m.transition_provenance().unwrap();
        assert!(p.is_direct());
        let TransitionProvenance::Direct { evidence } = p else {
            unreachable!()
        };
        assert!(evidence.contains(&Evidence::SidecarWrite));
        assert!(evidence.contains(&Evidence::ChildSpawned));
    }

    #[test]
    fn spawn_failed_is_direct_with_syscall_evidence() {
        let mut m = fresh_meta();
        m.transition_spawn_failed(EpochTimestamp::from_secs(1))
            .unwrap();
        let p = m.transition_provenance().unwrap();
        assert!(p.is_direct());
        let TransitionProvenance::Direct { evidence } = p else {
            unreachable!()
        };
        assert!(evidence.contains(&Evidence::SpawnFailedSyscall));
    }

    #[test]
    fn exited_is_direct_with_child_exit_observed() {
        let mut m = fresh_meta();
        m.transition_running(child()).unwrap();
        m.transition_exited(ExitReason::ExitedOk, EpochTimestamp::from_secs(2))
            .unwrap();
        let p = m.transition_provenance().unwrap();
        assert!(p.is_direct());
        let TransitionProvenance::Direct { evidence } = p else {
            unreachable!()
        };
        assert!(evidence.contains(&Evidence::ChildExitObserved));
    }

    #[test]
    fn dependency_failed_is_direct_with_dependency_evidence() {
        let mut m = fresh_meta();
        m.transition_dependency_failed(EpochTimestamp::from_secs(1), DepFailReason::Failed)
            .unwrap();
        let p = m.transition_provenance().unwrap();
        assert!(p.is_direct());
        let TransitionProvenance::Direct { evidence } = p else {
            unreachable!()
        };
        assert!(evidence.contains(&Evidence::DependencyFailed));
    }

    #[test]
    fn sidecar_lost_is_inferred_with_lock_and_non_terminal_evidence() {
        let mut m = fresh_meta();
        m.transition_running(child()).unwrap();
        m.reconcile_sidecar_lost(EpochTimestamp::from_secs(3))
            .unwrap();
        let p = m.transition_provenance().unwrap();
        assert!(p.is_inferred());
        let TransitionProvenance::Inferred { evidence } = p else {
            unreachable!()
        };
        assert!(evidence.contains(&Evidence::LockReleased));
        assert!(evidence.contains(&Evidence::NonTerminalMeta));
    }

    #[test]
    fn sidecar_lost_from_starting_also_inferred() {
        let mut m = fresh_meta();
        m.reconcile_sidecar_lost(EpochTimestamp::from_secs(1))
            .unwrap();
        assert!(m.transition_provenance().unwrap().is_inferred());
    }
}
