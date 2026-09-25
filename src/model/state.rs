use serde::{Deserialize, Serialize};
use std::num::NonZeroI32;

use super::dep_fail::DepFailReason;
use super::ids::{EpochTimestamp, ProcessIdentity};

/// Current status of a run. State-specific fields live inside the variants,
/// making invalid combinations unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum RunStatus {
    /// Sidecar started, child not yet spawned.
    Starting,
    /// Child is alive and being supervised.
    Running { child: ProcessIdentity },
    /// Child failed to spawn. No child identity exists.
    SpawnFailed { ended_at: EpochTimestamp },
    /// Run ended after child was running. Child identity preserved.
    Exited {
        child: ProcessIdentity,
        #[serde(flatten)]
        how: ExitReason,
        ended_at: EpochTimestamp,
    },
    /// Sidecar disappeared without writing terminal state.
    /// May or may not have had a child.
    SidecarLost {
        child: Option<ProcessIdentity>,
        ended_at: EpochTimestamp,
    },
    /// Dependency wait phase failed before child was spawned.
    DependencyFailed {
        ended_at: EpochTimestamp,
        #[serde(flatten)]
        reason: DepFailReason,
    },
}

/// How a running child exited. Only reachable from Running state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason")]
pub enum ExitReason {
    /// Child exited with code 0.
    ExitedOk,
    /// Child exited with non-zero code.
    ExitedError {
        #[serde(with = "nonzero_i32_serde")]
        code: NonZeroI32,
    },
    /// Child was killed gracefully (SIGTERM / cooperative shutdown).
    Killed,
    /// Child was force-killed (SIGKILL / TerminateJobObject).
    KilledForced,
    /// Child exceeded --timeout.
    TimedOut,
    /// The sidecar itself failed while supervising, killed the child, and
    /// recorded this directly. Contrast `RunStatus::SidecarLost`, which is
    /// inferred after the sidecar vanished without a word.
    SidecarFailed { step: SidecarStep },
}

/// The post-spawn step the sidecar was on when supervision failed. A stable
/// wire identifier (`"step": "output_log"`): add variants, never rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarStep {
    /// Writing the `child_pid` orphan breadcrumb.
    Breadcrumb,
    /// Creating the `--stdin` transport (FIFO / named pipe).
    StdinTransport,
    /// Binding the PTY attach socket.
    AttachBind,
    /// Persisting `Running` to `meta.json`.
    RunningMeta,
    /// Delivering readiness to the `start` client.
    Readiness,
    /// Opening `output.log` and capturing output.
    OutputLog,
    /// Waiting for the child's exit status.
    ChildWait,
}

impl SidecarStep {
    /// The wire name, as serialized.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Breadcrumb => "breadcrumb",
            Self::StdinTransport => "stdin_transport",
            Self::AttachBind => "attach_bind",
            Self::RunningMeta => "running_meta",
            Self::Readiness => "readiness",
            Self::OutputLog => "output_log",
            Self::ChildWait => "child_wait",
        }
    }

    /// Parse a wire name back into a step.
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Self> {
        [
            Self::Breadcrumb,
            Self::StdinTransport,
            Self::AttachBind,
            Self::RunningMeta,
            Self::Readiness,
            Self::OutputLog,
            Self::ChildWait,
        ]
        .into_iter()
        .find(|step| step.as_str() == name)
    }
}

impl std::fmt::Display for SidecarStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl RunStatus {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(self, RunStatus::Starting | RunStatus::Running { .. })
    }

    /// Get child identity if available.
    #[must_use]
    pub fn child(&self) -> Option<&ProcessIdentity> {
        match self {
            RunStatus::Starting
            | RunStatus::SpawnFailed { .. }
            | RunStatus::DependencyFailed { .. } => None,
            RunStatus::Running { child } | RunStatus::Exited { child, .. } => Some(child),
            RunStatus::SidecarLost { child, .. } => child.as_ref(),
        }
    }

    /// Get ended_at if terminal.
    #[must_use]
    pub fn ended_at(&self) -> Option<&EpochTimestamp> {
        match self {
            RunStatus::Starting | RunStatus::Running { .. } => None,
            RunStatus::SpawnFailed { ended_at }
            | RunStatus::Exited { ended_at, .. }
            | RunStatus::SidecarLost { ended_at, .. }
            | RunStatus::DependencyFailed { ended_at, .. } => Some(ended_at),
        }
    }
}

/// serde helper for NonZeroI32.
mod nonzero_i32_serde {
    use serde::{self, Deserialize, Deserializer, Serializer};
    use std::num::NonZeroI32;

    pub fn serialize<S>(value: &NonZeroI32, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i32(value.get())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<NonZeroI32, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = i32::deserialize(deserializer)?;
        NonZeroI32::new(value).ok_or_else(|| serde::de::Error::custom("exit code cannot be zero"))
    }
}
