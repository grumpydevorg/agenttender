//! Slice-2 read-path primitives — cursor codec, offset-aware segment
//! tailing, and output.log projection (spec §5.1–5.2).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use tempfile::TempDir;
use tender::events::{
    EventDraft, EventWriter, decode_cursor, encode_cursor, project_log_line, read_segment_records,
};
use tender::log::LogLine;
use tender::model::event::{EventTimestamp, Kind};
use tender::model::ids::{Namespace, RunId, SessionName, Source};

fn draft(kind: &str, data: serde_json::Value) -> EventDraft {
    EventDraft {
        kind: Kind::new(kind).unwrap(),
        namespace: Namespace::new("default").unwrap(),
        session: SessionName::new("s1").unwrap(),
        run_id: RunId::new(),
        generation: Some(1),
        source: Source::trusted("tender.sidecar").unwrap(),
        block_id: None,
        parent_id: None,
        data: Some(data),
    }
}

fn only_segment(session_dir: &Path) -> std::path::PathBuf {
    let events_dir = session_dir.join("events");
    let mut segs: Vec<_> = std::fs::read_dir(&events_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segs.sort();
    assert_eq!(segs.len(), 1);
    segs.remove(0)
}

// --- Cursor codec (spec §5.2) ---

#[test]
fn cursor_round_trips_streams() {
    let mut streams = BTreeMap::new();
    streams.insert("default/s1/events/aaa.jsonl".to_owned(), 0u64);
    streams.insert("default/s1/events/bbb.jsonl".to_owned(), 12345u64);
    streams.insert("other/s2/events/ccc.jsonl".to_owned(), u64::MAX);

    let token = encode_cursor(&streams);
    let decoded = decode_cursor(&token).unwrap();
    assert_eq!(decoded, streams);
}

#[test]
fn cursor_token_is_url_safe() {
    let mut streams = BTreeMap::new();
    // Enough entries that standard base64 would surely emit '+' or '/'.
    for i in 0..64 {
        streams.insert(format!("ns/s{i}/events/{i:04}.jsonl"), i * 7919);
    }
    let token = encode_cursor(&streams);
    assert!(
        token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "token must be URL-safe base64 without padding: {token}"
    );
}

#[test]
fn cursor_decode_rejects_garbage() {
    assert!(decode_cursor("not!!base64").is_err());
    assert!(decode_cursor("").is_err());
    // Valid base64, invalid JSON inside.
    assert!(decode_cursor("bm90LWpzb24").is_err());
}

#[test]
fn cursor_decode_rejects_unknown_version() {
    // {"v":2,"s":[]} — a future cursor version is treated as stale (exit 44
    // at the CLI layer), never silently accepted.
    use base64::Engine as _;
    let token =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"v":2,"s":[["a/b/events/c.jsonl",0]]}"#);
    assert!(decode_cursor(&token).is_err());
}

// --- Offset-aware segment reads (spec §5.1 follow) ---

#[test]
fn read_segment_records_returns_offsets_and_resumes_exactly() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut writer = EventWriter::new(&session_dir);
    writer
        .append(draft("build.one", serde_json::json!({"n": 1})), false)
        .unwrap();
    writer
        .append(draft("build.two", serde_json::json!({"n": 2})), false)
        .unwrap();
    let seg = only_segment(&session_dir);

    let outcome = read_segment_records(&seg, 0).unwrap();
    assert_eq!(outcome.records.len(), 2);
    assert_eq!(outcome.skipped, 0);
    assert_eq!(outcome.records[0].start, 0);
    assert_eq!(outcome.records[0].end, outcome.records[1].start);
    let file_len = std::fs::metadata(&seg).unwrap().len();
    assert_eq!(outcome.consumed_to, file_len);
    assert_eq!(outcome.records[1].end, file_len);

    // Resume from the mid-offset: only the second record.
    let resumed = read_segment_records(&seg, outcome.records[0].end).unwrap();
    assert_eq!(resumed.records.len(), 1);
    assert_eq!(resumed.records[0].event.kind.as_str(), "build.two");
    assert_eq!(resumed.consumed_to, file_len);

    // Resume from EOF: nothing.
    let empty = read_segment_records(&seg, file_len).unwrap();
    assert!(empty.records.is_empty());
    assert_eq!(empty.consumed_to, file_len);
}

#[test]
fn read_segment_records_leaves_torn_tail_unconsumed() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut writer = EventWriter::new(&session_dir);
    writer
        .append(draft("build.one", serde_json::json!({"n": 1})), false)
        .unwrap();
    let seg = only_segment(&session_dir);
    let complete_len = std::fs::metadata(&seg).unwrap().len();

    // A write in progress: no trailing newline yet.
    let mut f = std::fs::OpenOptions::new().append(true).open(&seg).unwrap();
    f.write_all(b"{\"v\":1,\"id\":\"partial").unwrap();
    drop(f);

    let outcome = read_segment_records(&seg, 0).unwrap();
    assert_eq!(outcome.records.len(), 1);
    assert_eq!(
        outcome.skipped, 0,
        "an unterminated tail line is in-progress, not torn"
    );
    assert_eq!(
        outcome.consumed_to, complete_len,
        "offset must stop before the unterminated tail"
    );

    // The writer finishes the line (still unparseable) — now it is a torn
    // line: consumed and counted.
    let mut f = std::fs::OpenOptions::new().append(true).open(&seg).unwrap();
    f.write_all(b"\n").unwrap();
    drop(f);
    let outcome = read_segment_records(&seg, complete_len).unwrap();
    assert!(outcome.records.is_empty());
    assert_eq!(outcome.skipped, 1);
    assert_eq!(outcome.consumed_to, std::fs::metadata(&seg).unwrap().len());
}

// --- output.log projection (spec §5.1 --include-logs) ---

#[test]
fn project_log_line_maps_stdout_and_stderr() {
    let line: LogLine = serde_json::from_value(serde_json::json!({
        "ts": 1751846400.25,
        "tag": "O",
        "content": "hello"
    }))
    .unwrap();
    let projected = project_log_line(&line, "default", "s1", "run-id-here").unwrap();
    assert_eq!(projected["kind"], "log.stdout");
    assert_eq!(projected["derived"], true);
    assert_eq!(projected["namespace"], "default");
    assert_eq!(projected["session"], "s1");
    assert_eq!(projected["run_id"], "run-id-here");
    assert_eq!(projected["source"], "tender.sidecar");
    assert_eq!(projected["data"]["content"], "hello");
    // Derived records carry no stored identity.
    assert!(projected.get("id").is_none());
    assert!(projected.get("writer").is_none());
    assert!(projected.get("seq").is_none());
    // f64 seconds converted to the envelope ts format.
    let ts = projected["ts"].as_str().unwrap();
    assert_eq!(ts, "2025-07-07T00:00:00.250000Z");
    assert!(ts.parse::<EventTimestamp>().is_ok());

    let err_line: LogLine = serde_json::from_value(serde_json::json!({
        "ts": 1751846400.5,
        "tag": "E",
        "content": "oops"
    }))
    .unwrap();
    let projected = project_log_line(&err_line, "default", "s1", "r").unwrap();
    assert_eq!(projected["kind"], "log.stderr");
}

#[test]
fn project_log_line_skips_annotations() {
    let line: LogLine = serde_json::from_value(serde_json::json!({
        "ts": 1751846400.0,
        "tag": "A",
        "content": {"source": "x.y", "event": "e"}
    }))
    .unwrap();
    assert!(project_log_line(&line, "default", "s1", "r").is_none());
}

// --- Flexible RFC 3339 parsing for --since ---

#[test]
fn timestamp_parse_flexible_accepts_common_utc_forms() {
    let full = EventTimestamp::parse_flexible("2026-07-07T01:02:03.456789Z").unwrap();
    assert_eq!(full.to_string(), "2026-07-07T01:02:03.456789Z");

    let no_fraction = EventTimestamp::parse_flexible("2026-07-07T01:02:03Z").unwrap();
    assert_eq!(no_fraction.to_string(), "2026-07-07T01:02:03.000000Z");

    let millis = EventTimestamp::parse_flexible("2026-07-07T01:02:03.5Z").unwrap();
    assert_eq!(millis.to_string(), "2026-07-07T01:02:03.500000Z");

    let nanos = EventTimestamp::parse_flexible("2026-07-07T01:02:03.123456789Z").unwrap();
    assert_eq!(nanos.to_string(), "2026-07-07T01:02:03.123456Z");
}

#[test]
fn timestamp_parse_flexible_rejects_non_utc_and_garbage() {
    assert!(EventTimestamp::parse_flexible("2026-07-07T01:02:03+02:00").is_err());
    assert!(EventTimestamp::parse_flexible("2026-07-07 01:02:03Z").is_err());
    assert!(EventTimestamp::parse_flexible("yesterday").is_err());
    assert!(EventTimestamp::parse_flexible("").is_err());
}

// --- Epoch conversions shared by watch re-backing and log projection ---

#[test]
fn timestamp_epoch_micros_round_trips_f64() {
    let ts = EventTimestamp::from_parts(1_751_846_400, 250_000);
    assert_eq!(ts.epoch_micros(), 1_751_846_400_250_000);

    let from_f64 = EventTimestamp::from_epoch_secs_f64(1_751_846_400.25);
    assert_eq!(from_f64, ts);
    // Sub-microsecond noise from f64 representation must not shift the
    // value (this literal is one ULP below .25 at this magnitude).
    let noisy = EventTimestamp::from_epoch_secs_f64(1_751_846_400.249_999_8);
    assert_eq!(noisy, ts);
}
