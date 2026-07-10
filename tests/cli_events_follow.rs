//! `tender events --follow` — poll-based live tailing with warm starts
//! (spec §5.1, slice 2 plan scope items 1–2).

mod harness;

use std::io::Write as _;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use harness::{tender, wait_running, wait_terminal};
use tempfile::TempDir;

/// Follower children + sleeper sessions are timing-sensitive; serialize
/// like cli_watch.rs does so parallel load can't blow the poll budgets.
static SERIAL: Mutex<()> = Mutex::new(());

fn spawn_events(root: &TempDir, args: &[&str]) -> Child {
    let bin = assert_cmd::cargo::cargo_bin("tender");
    Command::new(bin)
        .arg("events")
        .args(args)
        .env("HOME", root.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn tender events")
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

fn newest_segment(root: &TempDir, session: &str) -> std::path::PathBuf {
    let events_dir = root
        .path()
        .join(format!(".tender/sessions/default/{session}/events"));
    let mut segs: Vec<_> = std::fs::read_dir(&events_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segs.sort();
    segs.pop().expect("at least one segment")
}

#[test]
fn follow_from_now_surfaces_live_event_within_500ms() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tender(&root)
        .args(["start", "live", "--", "sleep", "5"])
        .assert()
        .success();
    wait_running(&root, "live");

    let child = spawn_events(&root, &["--follow", "--from-now"]);
    // Let the follower record EOF offsets and begin polling.
    std::thread::sleep(Duration::from_millis(400));

    tender(&root)
        .args(["emit", "--kind", "hook.live_probe", "--session", "live"])
        .assert()
        .success();
    // The acceptance budget: surfaced within 500 ms of the emit.
    std::thread::sleep(Duration::from_millis(500));

    let records = kill_and_read(child);
    assert!(
        records.iter().any(|r| r["kind"] == "hook.live_probe"),
        "live emit must surface within 500ms, got: {records:?}"
    );
    // --from-now skipped the session's pre-existing lifecycle history.
    assert!(
        records.iter().all(|r| r["kind"] != "run.starting"),
        "history must be skipped under --from-now, got: {records:?}"
    );
}

#[test]
fn follow_from_now_replays_later_discovered_sessions_from_start() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tender(&root)
        .args(["start", "old", "--", "echo", "old-hi"])
        .assert()
        .success();
    wait_terminal(&root, "old");

    let child = spawn_events(&root, &["--follow", "--from-now"]);
    std::thread::sleep(Duration::from_millis(400));

    tender(&root)
        .args(["start", "fresh", "--", "echo", "fresh-hi"])
        .assert()
        .success();
    wait_terminal(&root, "fresh");
    std::thread::sleep(Duration::from_millis(600));

    let records = kill_and_read(child);
    assert!(
        records.iter().all(|r| r["session"] != "old"),
        "pre-existing session history must be skipped, got: {records:?}"
    );
    let fresh_kinds: Vec<&str> = records
        .iter()
        .filter(|r| r["session"] == "fresh")
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        fresh_kinds,
        ["run.starting", "run.started", "run.exited"],
        "later-discovered sessions replay from their start"
    );
}

#[test]
fn follow_replays_history_then_streams_new_events() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tender(&root)
        .args(["start", "s1", "--", "sleep", "5"])
        .assert()
        .success();
    wait_running(&root, "s1");

    let child = spawn_events(&root, &["--follow"]);
    std::thread::sleep(Duration::from_millis(400));

    tender(&root)
        .args(["emit", "--kind", "test.after_replay", "--session", "s1"])
        .assert()
        .success();
    std::thread::sleep(Duration::from_millis(500));

    let records = kill_and_read(child);
    let kinds: Vec<&str> = records
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.starts_with(&["run.starting", "run.started"]),
        "history replays first, got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"test.after_replay"),
        "live events stream after replay, got: {kinds:?}"
    );
}

#[test]
fn follow_output_is_merge_ordered_by_ts_writer_seq() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tender(&root)
        .args(["start", "s1", "--", "sleep", "5"])
        .assert()
        .success();
    wait_running(&root, "s1");
    // A burst of events from distinct writers, all before the follower's
    // next poll — they arrive in one batch and must come out merge-ordered.
    for i in 0..5 {
        tender(&root)
            .args([
                "emit",
                "--kind",
                &format!("test.burst{i}"),
                "--session",
                "s1",
            ])
            .assert()
            .success();
    }

    let child = spawn_events(&root, &["--follow"]);
    std::thread::sleep(Duration::from_millis(500));
    let records = kill_and_read(child);

    assert!(records.len() >= 7, "lifecycle + burst, got: {records:?}");
    let keys: Vec<(String, String, u64)> = records
        .iter()
        .map(|r| {
            (
                r["ts"].as_str().unwrap().to_owned(),
                r["writer"].as_str().unwrap().to_owned(),
                r["seq"].as_u64().unwrap(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "batch output is (ts, writer, seq)-ordered");
}

#[test]
fn follow_picks_up_new_segments() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tender(&root)
        .args(["start", "s1", "--", "sleep", "5"])
        .assert()
        .success();
    wait_running(&root, "s1");

    let child = spawn_events(&root, &["--follow"]);
    std::thread::sleep(Duration::from_millis(400));

    // A new, lexicographically-later segment appears (rotation is slice 4,
    // but multi-segment logs are already legal). Its events must stream.
    let seg = newest_segment(&root, "s1");
    let first_line = std::fs::read_to_string(&seg)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    let mut event: serde_json::Value = serde_json::from_str(&first_line).unwrap();
    event["kind"] = "test.from_new_segment".into();
    let new_seg = seg.with_file_name("zzzz-pickup.jsonl");
    let mut f = std::fs::File::create(&new_seg).unwrap();
    writeln!(f, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    drop(f);

    std::thread::sleep(Duration::from_millis(500));
    let records = kill_and_read(child);
    assert!(
        records.iter().any(|r| r["kind"] == "test.from_new_segment"),
        "events in new segments must be picked up, got: {records:?}"
    );
}

#[test]
fn follow_strict_exits_65_on_first_observed_parse_skip() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tender(&root)
        .args(["start", "s1", "--", "sleep", "5"])
        .assert()
        .success();
    wait_running(&root, "s1");

    let mut child = spawn_events(&root, &["--follow", "--strict"]);
    std::thread::sleep(Duration::from_millis(400));

    // A complete-but-unparseable line lands in the newest segment.
    let seg = newest_segment(&root, "s1");
    let mut f = std::fs::OpenOptions::new().append(true).open(&seg).unwrap();
    f.write_all(b"{\"v\":1,\"torn\n").unwrap();
    drop(f);

    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "--strict follower must exit on the parse skip"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(65));
}
