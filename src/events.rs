//! Per-session event log — append path (spec §3.2–3.5) and read path (§5)
//! of docs/plans/specs/event-protocol.md.
//!
//! Layout, per session dir:
//! ```text
//! events/
//!   <seg-uuidv7>.jsonl   # event segments — HISTORY authority
//!   blobs/<sha256>       # spilled payloads, content-addressed
//!   append.lock          # advisory lock file (POSIX only)
//! ```

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::model::dep_fail::DepFailReason;
use crate::model::event::{DataRef, ENVELOPE_VERSION, Event, EventTimestamp, Kind, Uuid7};
use crate::model::ids::{Namespace, RunId, SessionName, Source};
use crate::model::state::{ExitReason, RunStatus};

/// Inline `data` cap; larger payloads spill to a blob (spec §1, §3.4).
pub const MAX_INLINE_DATA_BYTES: usize = 16 * 1024;
/// Preview size for spilled/degraded payloads (spec §1 `data_ref`).
pub const MAX_PREVIEW_BYTES: usize = 4 * 1024;

/// The fields a caller supplies; `EventWriter::append` stamps the rest
/// (`v`, `id`, `ts`, `writer`, `seq`) and handles oversize spill.
#[derive(Debug, Clone)]
pub struct EventDraft {
    pub kind: Kind,
    pub namespace: Namespace,
    pub session: SessionName,
    pub run_id: RunId,
    pub generation: Option<u64>,
    pub source: Source,
    pub block_id: Option<Uuid7>,
    pub parent_id: Option<Uuid7>,
    pub data: Option<serde_json::Value>,
}

/// Appends events to one session's `events/` dir with a stable writer
/// identity and per-writer contiguous `seq` starting at 1.
///
/// `seq` is consumed even if an append fails: a gap in the log is honest
/// (readers detect loss); a reused `seq` would be a lie.
#[derive(Debug)]
pub struct EventWriter {
    session_dir: PathBuf,
    writer: Uuid7,
    next_seq: u64,
}

impl EventWriter {
    /// A writer with a freshly minted identity (CLI emitters).
    #[must_use]
    pub fn new(session_dir: &Path) -> Self {
        Self::with_writer(session_dir, Uuid7::new())
    }

    /// A writer with a caller-supplied identity (the sidecar uses its run id).
    #[must_use]
    pub fn with_writer(session_dir: &Path, writer: Uuid7) -> Self {
        Self {
            session_dir: session_dir.to_path_buf(),
            writer,
            next_seq: 1,
        }
    }

    #[must_use]
    pub fn writer_id(&self) -> Uuid7 {
        self.writer
    }

    /// Stamp a draft into a full envelope and append it as one line.
    /// `durable` forces the segment to stable storage before returning
    /// (`fdatasync`; `F_FULLFSYNC` on macOS via std).
    ///
    /// # Errors
    /// Returns the underlying IO error; the payload is never silently dropped
    /// short of the append itself failing (blob failure degrades inline, §3.4).
    pub fn append(&mut self, draft: EventDraft, durable: bool) -> io::Result<Event> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let (data, data_ref, truncated) = prepare_data(&self.session_dir, draft.data);
        let event = Event {
            v: ENVELOPE_VERSION,
            id: Uuid7::new(),
            ts: EventTimestamp::now(),
            kind: draft.kind,
            namespace: draft.namespace,
            session: draft.session,
            run_id: draft.run_id,
            generation: draft.generation,
            writer: self.writer,
            seq,
            source: draft.source,
            block_id: draft.block_id,
            parent_id: draft.parent_id,
            data,
            data_ref,
            truncated,
        };
        append_line(&self.session_dir, &event, durable)?;
        Ok(event)
    }
}

/// Stamp a draft into a full envelope without a session dir — the lost+found
/// path (spec §7). Fresh writer identity, `seq` 1; oversize data is
/// inline-truncated because there is no blob store to spill to.
#[must_use]
pub fn stamp_orphan_event(draft: EventDraft) -> Event {
    let (data, truncated) = match draft.data {
        None => (None, None),
        Some(value) => match serde_json::to_vec(&value) {
            Ok(bytes) if bytes.len() > MAX_INLINE_DATA_BYTES => {
                (Some(preview_of(&bytes)), Some(true))
            }
            _ => (Some(value), None),
        },
    };
    Event {
        v: ENVELOPE_VERSION,
        id: Uuid7::new(),
        ts: EventTimestamp::now(),
        kind: draft.kind,
        namespace: draft.namespace,
        session: draft.session,
        run_id: draft.run_id,
        generation: draft.generation,
        writer: Uuid7::new(),
        seq: 1,
        source: draft.source,
        block_id: draft.block_id,
        parent_id: draft.parent_id,
        data,
        data_ref: None,
        truncated,
    }
}

/// Spill oversize `data` per spec §3.4. Returns `(data, data_ref, truncated)`:
/// - fits inline → unchanged
/// - oversize, blob written → preview + `data_ref` + `truncated:true`
/// - oversize, blob write failed → preview + `truncated:true` (never a drop)
fn prepare_data(
    session_dir: &Path,
    data: Option<serde_json::Value>,
) -> (Option<serde_json::Value>, Option<DataRef>, Option<bool>) {
    let Some(data) = data else {
        return (None, None, None);
    };
    let serialized = match serde_json::to_vec(&data) {
        Ok(bytes) => bytes,
        Err(_) => return (Some(data), None, None), // Value serialization cannot fail in practice
    };
    if serialized.len() <= MAX_INLINE_DATA_BYTES {
        return (Some(data), None, None);
    }

    let preview = preview_of(&serialized);
    match write_blob(session_dir, &serialized) {
        Ok(data_ref) => (Some(preview), Some(data_ref), Some(true)),
        Err(_) => (Some(preview), None, Some(true)),
    }
}

/// A ≤4 KiB preview object for a spilled payload: the head of the
/// serialized JSON as an opaque string (arbitrary JSON cannot be truncated
/// to valid JSON in place).
fn preview_of(serialized: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(serialized);
    let mut end = MAX_PREVIEW_BYTES.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    serde_json::json!({ "preview": &text[..end] })
}

/// Write a content-addressed blob under `events/blobs/<sha256>` via
/// temp + rename in the same directory. Identical payloads dedupe by key.
fn write_blob(session_dir: &Path, serialized: &[u8]) -> io::Result<DataRef> {
    let sha256 = hex(&Sha256::digest(serialized));
    let blobs_dir = session_dir.join("events").join("blobs");
    std::fs::create_dir_all(&blobs_dir)?;

    let final_path = blobs_dir.join(&sha256);
    if !final_path.exists() {
        let tmp_path = blobs_dir.join(format!(".tmp-{}", Uuid7::new()));
        let mut tmp = File::create(&tmp_path)?;
        if let Err(e) = tmp.write_all(serialized) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        drop(tmp);
        std::fs::rename(&tmp_path, &final_path)?;
    }

    Ok(DataRef {
        path: format!("events/blobs/{sha256}"),
        bytes: serialized.len() as u64,
        sha256,
        media_type: "application/json".to_owned(),
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Append one serialized event line — the whole spec §3.2 protocol:
/// newest-segment pick / `create_new` race handling, `append(true)` open,
/// advisory flock on `events/append.lock` (POSIX only; Windows relies on
/// the documented `FILE_APPEND_DATA` atomic-append contract), single
/// `write_all`, optional fdatasync.
fn append_line(session_dir: &Path, event: &Event, durable: bool) -> io::Result<()> {
    let events_dir = session_dir.join("events");
    std::fs::create_dir_all(&events_dir)?;

    let mut line = serde_json::to_string(event).map_err(io::Error::other)?;
    line.push('\n');

    let mut file = open_segment(&events_dir)?;
    let _lock = SegmentLock::acquire(&events_dir)?;
    file.write_all(line.as_bytes())?;
    if durable {
        file.sync_data()?;
    }
    Ok(())
}

/// Open the newest segment, creating the first one on demand.
/// `create_new` losers re-list and open the winner's segment.
fn open_segment(events_dir: &Path) -> io::Result<File> {
    for _ in 0..8 {
        if let Some(newest) = newest_segment(events_dir)? {
            match OpenOptions::new().append(true).open(&newest) {
                Ok(f) => return Ok(f),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // pruned mid-pick
                Err(e) => return Err(e),
            }
        }
        let candidate = events_dir.join(format!("{}.jsonl", Uuid7::new()));
        match OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&candidate)
        {
            Ok(f) => return Ok(f),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue, // race loser
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(
        "could not open an event segment after repeated races",
    ))
}

/// The lexicographically greatest `events/*.jsonl` — UUIDv7 names sort by
/// creation time, so greatest = newest.
fn newest_segment(events_dir: &Path) -> io::Result<Option<PathBuf>> {
    let mut newest: Option<PathBuf> = None;
    for entry in std::fs::read_dir(events_dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|x| x == "jsonl")
            && newest.as_ref().is_none_or(|n| path > *n)
        {
            newest = Some(path);
        }
    }
    Ok(newest)
}

/// Advisory exclusive lock on `events/append.lock` for the duration of one
/// append. A dedicated lock file, never the data file — readers never take
/// it and are never blocked. POSIX only: on Windows `OpenOptions::append`
/// strips `FILE_WRITE_DATA`, which gives a documented per-WriteFile
/// atomic-append contract across processes; taking `LockFileEx` there would
/// be mandatory and block tailers (spec §3.2).
struct SegmentLock {
    #[cfg(unix)]
    _file: File,
}

impl SegmentLock {
    #[cfg(unix)]
    fn acquire(events_dir: &Path) -> io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        let file = File::create(events_dir.join("append.lock"))?;
        // SAFETY: file is an open File, so as_raw_fd() returns a valid fd.
        // LOCK_EX is a valid blocking-exclusive flock operation.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        // flock releases on close — dropping the File unlocks.
        Ok(Self { _file: file })
    }

    #[cfg(windows)]
    fn acquire(_events_dir: &Path) -> io::Result<Self> {
        Ok(Self {})
    }
}

/// Lifecycle event kind for a run status — the shipped watch vocabulary,
/// reused verbatim (spec §1).
#[must_use]
pub fn lifecycle_kind(status: &RunStatus) -> Kind {
    let name = match status {
        RunStatus::Starting => "run.starting",
        RunStatus::Running { .. } => "run.started",
        RunStatus::SpawnFailed { .. } => "run.spawn_failed",
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk | ExitReason::ExitedError { .. } => "run.exited",
            ExitReason::Killed | ExitReason::KilledForced => "run.killed",
            ExitReason::TimedOut => "run.timed_out",
        },
        RunStatus::SidecarLost { .. } => "run.sidecar_lost",
        RunStatus::DependencyFailed { .. } => "run.dependency_failed",
    };
    Kind::new(name).expect("lifecycle kinds satisfy the kind grammar")
}

/// Lifecycle event `data`: watch's data shape (kept intact — watch output is
/// a frozen compat surface) plus `provenance` per spec §1 example (a) and
/// the §4 one-provenance-vocabulary rule.
#[must_use]
pub fn lifecycle_data(status: &RunStatus, provenance: &str) -> serde_json::Value {
    let mut data = match status {
        RunStatus::Starting => serde_json::json!({"status": "Starting"}),
        RunStatus::Running { .. } => serde_json::json!({"status": "Running"}),
        RunStatus::SpawnFailed { .. } => serde_json::json!({"status": "SpawnFailed"}),
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk => {
                serde_json::json!({"status": "Exited", "reason": "ExitedOk", "exit_code": 0})
            }
            ExitReason::ExitedError { code } => {
                serde_json::json!({"status": "Exited", "reason": "ExitedError", "exit_code": code.get()})
            }
            ExitReason::Killed => serde_json::json!({"status": "Exited", "reason": "Killed"}),
            ExitReason::KilledForced => {
                serde_json::json!({"status": "Exited", "reason": "KilledForced"})
            }
            ExitReason::TimedOut => {
                serde_json::json!({"status": "Exited", "reason": "TimedOut"})
            }
        },
        RunStatus::SidecarLost { .. } => serde_json::json!({"status": "SidecarLost"}),
        RunStatus::DependencyFailed { reason, .. } => {
            let reason_str = match reason {
                DepFailReason::Failed => "Failed",
                DepFailReason::TimedOut => "TimedOut",
                DepFailReason::Killed => "Killed",
                DepFailReason::KilledForced => "KilledForced",
            };
            serde_json::json!({"status": "DependencyFailed", "reason": reason_str})
        }
    };
    data["provenance"] = serde_json::Value::String(provenance.to_owned());
    data
}

/// Result of reading a session's event log.
#[derive(Debug)]
pub struct ReadOutcome {
    /// Events merged by `(ts, writer, seq)` (spec §4 deterministic merge).
    pub events: Vec<Event>,
    /// Lines that failed to parse (torn writes, foreign fragments).
    /// JSONL is self-synchronizing: readers resync at the next newline.
    pub skipped: usize,
}

/// Deterministic cross-writer merge key (spec §4): `(ts, writer, seq)`.
/// UUID `u128` ordering matches the canonical string ordering.
#[must_use]
pub fn merge_key(event: &Event) -> (EventTimestamp, u128, u64) {
    (event.ts, event.writer.as_uuid().as_u128(), event.seq)
}

/// Read every segment of one session's event log, in segment-name order,
/// then merge by `(ts, writer, seq)`. A missing `events/` dir is an empty
/// log, not an error.
///
/// # Errors
/// Returns IO errors from directory listing or file reads; parse failures
/// are counted in `skipped`, never fatal.
pub fn read_session_events(session_dir: &Path) -> io::Result<ReadOutcome> {
    let events_dir = session_dir.join("events");
    let mut outcome = ReadOutcome {
        events: Vec::new(),
        skipped: 0,
    };
    if !events_dir.exists() {
        return Ok(outcome);
    }

    let mut segments: Vec<PathBuf> = std::fs::read_dir(&events_dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segments.sort();

    for segment in segments {
        // Byte-level resync: JSONL is self-synchronizing (spec §3.2 defense
        // in depth). A non-UTF8 fragment is a counted skip like any other
        // unparseable line — never a fatal error for the whole replay.
        let content = std::fs::read(&segment)?;
        for raw_line in content.split(|b| *b == b'\n') {
            let raw_line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            if raw_line.is_empty() {
                continue;
            }
            let parsed = std::str::from_utf8(raw_line)
                .ok()
                .and_then(|line| serde_json::from_str::<Event>(line).ok());
            match parsed {
                Some(event) => outcome.events.push(event),
                None => outcome.skipped += 1,
            }
        }
    }

    outcome.events.sort_by_key(merge_key);
    Ok(outcome)
}

/// Append a fully-addressed event to `<tender-root>/lost+found/events.jsonl`
/// (spec §7): emits from a process whose session dir was pruned or replaced
/// keep their data without resurrecting the session dir. Swept by `prune`.
///
/// # Errors
/// Returns IO errors from creating or appending to the lost+found log.
pub fn append_lost_found(tender_root: &Path, event: &Event) -> io::Result<()> {
    let dir = tender_root.join("lost+found");
    std::fs::create_dir_all(&dir)?;

    let mut line = serde_json::to_string(event).map_err(io::Error::other)?;
    line.push('\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("events.jsonl"))?;
    let _lock = SegmentLock::acquire(&dir)?;
    file.write_all(line.as_bytes())
}
