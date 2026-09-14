use serde::{Deserialize, Serialize};

/// Who currently controls input to a PTY session.
///
/// Slice one rules (no agent lease):
/// - start --pty → AgentControl
/// - attach → HumanControl (steals from AgentControl)
/// - human detach → AgentControl (always, no lease check)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PtyControl {
    /// Agent owns input — push is accepted.
    AgentControl,
    /// Human is attached — push is rejected, terminal relay is active.
    HumanControl,
}

/// PTY session metadata. Present only for PTY-enabled sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyMeta {
    pub enabled: bool,
    pub control: PtyControl,
    /// The run's exact recording, when one was started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<PtyRecording>,
}

impl PtyMeta {
    pub fn new() -> Self {
        Self {
            enabled: true,
            control: PtyControl::AgentControl,
            recording: None,
        }
    }
}

/// A PTY run's recording policy and state, as the sidecar last reported them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyRecording {
    /// The segment directory, relative to the session directory.
    pub dir: String,
    /// Whether input bytes are recorded. Output and geometry always are.
    pub input_recorded: bool,
    #[serde(flatten)]
    pub state: RecordingState,
}

/// Where a recording stands. Sequences are recording sequence numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum RecordingState {
    /// Records are being appended.
    Recording,
    /// The run ended with everything it produced recorded.
    Complete {
        last_recorded_sequence: Option<u64>,
        last_synced_sequence: Option<u64>,
    },
    /// Recording ended before the run did. Nothing after
    /// `last_recorded_sequence` is recorded.
    Stopped {
        last_recorded_sequence: Option<u64>,
        /// Known once the run has ended.
        last_synced_sequence: Option<u64>,
        reason: RecordingStopReason,
        /// The storage error, for [`RecordingStopReason::WriteFailed`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Why a recording stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStopReason {
    /// The run reached its recording size limit.
    SizeLimit,
    /// Storage rejected a write, sync, or segment publication.
    WriteFailed,
    /// Storage fell too far behind the output.
    BacklogFull,
    /// Storage had not caught up when the run ended.
    Stalled,
}

impl Default for PtyMeta {
    fn default() -> Self {
        Self::new()
    }
}
