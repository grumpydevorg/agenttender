//! `tender watch` re-backed by the event log (spec §5.3, slice 2 plan
//! scope item 7). Output shape frozen — these tests assert the *gains*
//! (un-collapsed transitions, true timestamps, provenance stripped) and
//! the legacy fallback; cli_watch.rs continues to pin the frozen shape.

mod harness;

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use harness::{tender, wait_terminal};
use tempfile::TempDir;
use tender::model::event::EventTimestamp;

static SERIAL: Mutex<()> = Mutex::new(());

fn spawn_watch(root: &TempDir, args: &[&str]) -> Child {
    let bin = assert_cmd::cargo::cargo_bin("tender");
    Command::new(bin)
        .arg("watch")
        .args(args)
        .env("HOME", root.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn tender watch")
}

fn kill_and_read(mut child: Child) -> Vec<serde_json::Value> {
    let _ = child.kill();
    let output = child.wait_with_output().expect("failed to wait on child");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("NDJSON line parses"))
        .collect()
}

fn run_names(records: &[serde_json::Value], session: &str) -> Vec<String> {
    records
        .iter()
        .filter(|r| r["kind"] == "run" && r["session"] == session)
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn fast_exit_session_shows_all_three_transitions_uncollapsed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Watch starts BEFORE the session exists.
    let child = spawn_watch(&root, &["--events"]);
    std::thread::sleep(Duration::from_millis(400));

    // A session that exits faster than any meta-diff poll could observe.
    tender(&root)
        .args(["start", "fast", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "fast");
    std::thread::sleep(Duration::from_millis(800));

    let records = kill_and_read(child);
    assert_eq!(
        run_names(&records, "fast"),
        ["run.starting", "run.started", "run.exited"],
        "event-log backing un-collapses fast transitions, got: {records:?}"
    );

    // Frozen shape, real payloads: f64 ts, kind/name split, legacy data —
    // and the event's provenance field stripped at projection.
    for record in records.iter().filter(|r| r["kind"] == "run") {
        assert!(record["ts"].is_f64() || record["ts"].is_u64());
        assert_eq!(record["source"], "tender.sidecar");
        assert!(record["data"]["status"].is_string());
        assert!(
            record["data"].get("provenance").is_none(),
            "provenance is event-log detail, not watch surface: {record}"
        );
    }
}

#[test]
fn preexisting_session_still_gets_current_state_snapshot_only() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tender(&root)
        .args(["start", "done", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "done");

    let child = spawn_watch(&root, &["--events"]);
    std::thread::sleep(Duration::from_millis(700));
    let records = kill_and_read(child);

    assert_eq!(
        run_names(&records, "done"),
        ["run.exited"],
        "pre-existing sessions keep watch's snapshot contract, got: {records:?}"
    );
}

#[test]
fn rebacked_snapshot_carries_the_true_timestamp() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tender(&root)
        .args(["start", "stamped", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "stamped");

    // The stored run.exited event's occurrence time.
    let replay = tender(&root)
        .args(["events", "--session", "stamped", "--kind", "run.exited"])
        .output()
        .unwrap();
    let stored: serde_json::Value = serde_json::from_str(
        String::from_utf8_lossy(&replay.stdout).lines().next().unwrap(),
    )
    .unwrap();
    let stored_micros = stored["ts"]
        .as_str()
        .unwrap()
        .parse::<EventTimestamp>()
        .unwrap()
        .epoch_micros();

    // Watch starts later; its snapshot must carry the occurrence time, not
    // poll-detection time.
    std::thread::sleep(Duration::from_millis(300));
    let child = spawn_watch(&root, &["--events"]);
    std::thread::sleep(Duration::from_millis(700));
    let records = kill_and_read(child);

    let snapshot = records
        .iter()
        .find(|r| r["kind"] == "run" && r["session"] == "stamped")
        .expect("snapshot emitted");
    let watch_micros = (snapshot["ts"].as_f64().unwrap() * 1e6).round() as u64;
    let drift = watch_micros.abs_diff(stored_micros);
    assert!(
        drift <= 2,
        "watch ts must be the event's occurrence time (drift {drift}µs)"
    );
}

#[test]
fn session_without_events_dir_keeps_legacy_meta_diff_synthesis() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tender(&root)
        .args(["start", "legacy", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "legacy");

    // A pre-slice-1 layout: meta.json + output.log, no events dir.
    let events_dir = root.path().join(".tender/sessions/default/legacy/events");
    std::fs::remove_dir_all(&events_dir).unwrap();

    let child = spawn_watch(&root, &["--events"]);
    std::thread::sleep(Duration::from_millis(700));
    let records = kill_and_read(child);

    assert_eq!(
        run_names(&records, "legacy"),
        ["run.exited"],
        "meta-diff snapshot for sessions without an event log, got: {records:?}"
    );
    let snapshot = &records[0];
    assert_eq!(snapshot["kind"], "run");
    assert_eq!(snapshot["source"], "tender.sidecar");
    assert!(snapshot["ts"].is_f64() || snapshot["ts"].is_u64());
    assert_eq!(snapshot["data"]["status"], "Exited");
}

#[test]
fn from_now_skips_existing_but_streams_new_sessions_uncollapsed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tender(&root)
        .args(["start", "old", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "old");

    let child = spawn_watch(&root, &["--events", "--from-now"]);
    std::thread::sleep(Duration::from_millis(400));

    tender(&root)
        .args(["start", "fresh", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "fresh");
    std::thread::sleep(Duration::from_millis(800));

    let records = kill_and_read(child);
    assert!(
        run_names(&records, "old").is_empty(),
        "--from-now skips pre-existing sessions, got: {records:?}"
    );
    assert_eq!(
        run_names(&records, "fresh"),
        ["run.starting", "run.started", "run.exited"],
        "sessions started after watch replay their full history"
    );
}

#[test]
fn replaced_session_streams_the_new_generation_uncollapsed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tender(&root)
        .args(["start", "swap", "--", "echo", "first"])
        .assert()
        .success();
    wait_terminal(&root, "swap");

    let child = spawn_watch(&root, &["--events"]);
    std::thread::sleep(Duration::from_millis(500));

    tender(&root)
        .args(["start", "swap", "--replace", "--", "echo", "second"])
        .assert()
        .success();
    wait_terminal(&root, "swap");
    std::thread::sleep(Duration::from_millis(800));

    let records = kill_and_read(child);
    let names = run_names(&records, "swap");
    // Snapshot of gen 1, then gen 2's full lifecycle from its fresh log.
    assert_eq!(
        names,
        [
            "run.exited",
            "run.starting",
            "run.started",
            "run.exited"
        ],
        "got: {records:?}"
    );
}
