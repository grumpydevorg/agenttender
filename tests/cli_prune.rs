mod harness;

use harness::{tendr, wait_running};
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// Start a session that exits immediately, wait for terminal state.
fn create_terminal_session(root: &TempDir, name: &str, namespace: &str) {
    let out = tendr(root)
        .args([
            "start",
            name,
            "--namespace",
            namespace,
            "--",
            "echo",
            "done",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Terminal metadata is written just before the sidecar releases its lock, and
    // prune skips a locked session (#86). Wait until the lock can be taken, then
    // release it so the test's prune can acquire it.
    drop(harness::wait_terminal_quiescent_ns(root, namespace, name));
}

/// Parse NDJSON stdout into a Vec of serde_json::Value.
fn parse_ndjson(stdout: &[u8]) -> Vec<serde_json::Value> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("each line should be valid JSON"))
        .collect()
}

/// Backdate ended_at in a session's meta.json by rewriting the file.
fn backdate_ended_at(root: &TempDir, namespace: &str, session: &str, age_secs: u64) {
    let meta_path = root
        .path()
        .join(format!(".tendr/sessions/{namespace}/{session}/meta.json"));
    let content = std::fs::read_to_string(&meta_path).unwrap();
    let mut meta: serde_json::Value = serde_json::from_str(&content).unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let old_ts = now - age_secs;
    meta["ended_at"] = serde_json::Value::from(old_ts);

    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();
}

#[test]
fn prune_deletes_terminal_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-del", "default");

    let session_dir = root.path().join(".tendr/sessions/default/prune-del");
    assert!(
        session_dir.exists(),
        "session dir should exist before prune"
    );

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(
        out.status.success(),
        "prune failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !session_dir.exists(),
        "session dir should be removed after prune"
    );

    let lines = parse_ndjson(&out.stdout);
    assert!(
        lines.len() >= 2,
        "expected at least session + summary lines"
    );

    let session_line = &lines[0];
    assert_eq!(session_line["type"], "session");
    assert_eq!(session_line["action"], "delete");
    assert_eq!(session_line["session"], "prune-del");
}

#[test]
fn prune_skips_running_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .args(["start", "prune-running", "--", "sleep", "60"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let _session = harness::SessionGuard::new(&root, "prune-running");
    wait_running(&root, "prune-running");

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(out.status.success());

    let lines = parse_ndjson(&out.stdout);
    let session_line = lines.iter().find(|l| l["type"] == "session").unwrap();
    assert_eq!(session_line["action"], "skip");
    // Running sessions have the sidecar lock held, so they're skipped as "locked"
    // (lock check comes before meta read per invariant table)
    assert_eq!(session_line["skip_reason"], "locked");

    let session_dir = root.path().join(".tendr/sessions/default/prune-running");
    assert!(
        session_dir.exists(),
        "running session dir should still exist"
    );
}

#[test]
fn prune_respects_older_than() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-old", "default");
    create_terminal_session(&root, "prune-recent", "default");

    // Backdate "prune-old" to 8 days ago
    backdate_ended_at(&root, "default", "prune-old", 8 * 24 * 3600);

    let out = tendr(&root)
        .args(["prune", "--older-than", "7d"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let old_dir = root.path().join(".tendr/sessions/default/prune-old");
    let recent_dir = root.path().join(".tendr/sessions/default/prune-recent");
    assert!(!old_dir.exists(), "old session should be deleted");
    assert!(recent_dir.exists(), "recent session should be kept");

    let lines = parse_ndjson(&out.stdout);
    let actions: Vec<&str> = lines
        .iter()
        .filter(|l| l["type"] == "session")
        .map(|l| l["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"delete"), "should have a delete action");
    assert!(actions.contains(&"skip"), "should have a skip action");

    // Verify too_recent skip includes ended_at
    let skip_line = lines
        .iter()
        .find(|l| l["type"] == "session" && l["action"] == "skip")
        .unwrap();
    assert_eq!(skip_line["skip_reason"], "too_recent");
    assert!(
        skip_line.get("ended_at").is_some() && !skip_line["ended_at"].is_null(),
        "too_recent skip should include ended_at"
    );
}

#[test]
fn prune_dry_run_preserves_sessions() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-dry", "default");

    let session_dir = root.path().join(".tendr/sessions/default/prune-dry");

    let out = tendr(&root)
        .args(["prune", "--all", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success());

    assert!(
        session_dir.exists(),
        "session dir should still exist after dry-run"
    );

    let lines = parse_ndjson(&out.stdout);
    let session_line = lines.iter().find(|l| l["type"] == "session").unwrap();
    assert_eq!(session_line["action"], "delete");

    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();
    assert_eq!(summary["dry_run"], true);
    assert_eq!(summary["deleted"], 1);
}

#[test]
fn prune_without_filter_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root).args(["prune"]).output().unwrap();
    assert!(!out.status.success(), "prune without filter should fail");
}

#[test]
fn prune_skips_corrupt_meta() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Create a session dir with garbage meta.json
    let session_dir = root.path().join(".tendr/sessions/default/prune-corrupt");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("meta.json"), "not valid json {{{").unwrap();

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(out.status.success());

    assert!(
        session_dir.exists(),
        "corrupt session dir should still exist"
    );

    let lines = parse_ndjson(&out.stdout);
    let session_line = lines.iter().find(|l| l["type"] == "session").unwrap();
    assert_eq!(session_line["action"], "skip");
    assert_eq!(session_line["skip_reason"], "corrupt_meta");
}

#[test]
fn prune_skips_missing_meta() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Create a session dir with no meta.json
    let session_dir = root.path().join(".tendr/sessions/default/prune-nometa");
    std::fs::create_dir_all(&session_dir).unwrap();

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(out.status.success());

    assert!(
        session_dir.exists(),
        "missing-meta session dir should still exist"
    );

    let lines = parse_ndjson(&out.stdout);
    let session_line = lines.iter().find(|l| l["type"] == "session").unwrap();
    assert_eq!(session_line["action"], "skip");
    assert_eq!(session_line["skip_reason"], "missing_meta");
}

#[test]
fn prune_respects_namespace() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-ns", "ns-a");
    create_terminal_session(&root, "prune-ns", "ns-b");

    let out = tendr(&root)
        .args(["prune", "--all", "--namespace", "ns-a"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let dir_a = root.path().join(".tendr/sessions/ns-a/prune-ns");
    let dir_b = root.path().join(".tendr/sessions/ns-b/prune-ns");
    assert!(!dir_a.exists(), "ns-a session should be deleted");
    assert!(dir_b.exists(), "ns-b session should be untouched");

    let lines = parse_ndjson(&out.stdout);
    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();
    assert_eq!(summary["namespace"], "ns-a");
    assert_eq!(summary["deleted"], 1);
}

#[test]
fn prune_output_format() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-fmt", "default");

    let out = tendr(&root)
        .args(["prune", "--all", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let lines = parse_ndjson(&out.stdout);
    assert!(lines.len() >= 2, "need at least session + summary lines");

    // Every line has a type field
    for line in &lines {
        assert!(
            line.get("type").is_some(),
            "every line must have a type field: {line}"
        );
    }

    // Last line is summary
    let last = lines.last().unwrap();
    assert_eq!(last["type"], "summary");

    // Non-last lines are sessions
    for line in &lines[..lines.len() - 1] {
        assert_eq!(line["type"], "session");
    }
}

#[test]
fn prune_summary_counts_match_mixed_outcomes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // 1. Deletable terminal session
    create_terminal_session(&root, "prune-mix-ok", "default");

    // 2. Running session (will be skipped)
    let out = tendr(&root)
        .args(["start", "prune-mix-run", "--", "sleep", "60"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let _session = harness::SessionGuard::new(&root, "prune-mix-run");
    wait_running(&root, "prune-mix-run");

    // 3. Corrupt meta session (will be skipped)
    let corrupt_dir = root.path().join(".tendr/sessions/default/prune-mix-bad");
    std::fs::create_dir_all(&corrupt_dir).unwrap();
    std::fs::write(corrupt_dir.join("meta.json"), "garbage").unwrap();

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(out.status.success());

    let lines = parse_ndjson(&out.stdout);
    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();

    let deleted = summary["deleted"].as_u64().unwrap();
    let skipped = summary["skipped"].as_u64().unwrap();
    let failed = summary["failed"].as_u64().unwrap();

    assert_eq!(deleted, 1, "one terminal session should be deleted");
    assert_eq!(skipped, 2, "running + corrupt should be skipped");
    assert_eq!(failed, 0, "no failures expected");

    let session_lines: Vec<_> = lines.iter().filter(|l| l["type"] == "session").collect();
    assert_eq!(
        session_lines.len() as u64,
        deleted + skipped + failed,
        "session line count must match summary totals"
    );
}

#[test]
#[cfg(unix)]
fn prune_delete_failure_reports_error_and_continues() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Create two deletable terminal sessions
    create_terminal_session(&root, "prune-err-a", "default");
    create_terminal_session(&root, "prune-err-b", "default");

    // Make one session's directory unremovable by removing write permission on parent
    // Actually, make the session dir itself unreadable so remove_dir_all fails
    let dir_a = root.path().join(".tendr/sessions/default/prune-err-a");
    // Create a subdirectory and make it unremovable
    let blocker = dir_a.join("blocker");
    std::fs::create_dir(&blocker).unwrap();
    std::fs::write(blocker.join("file"), "data").unwrap();
    // Remove write+execute on the blocker dir so its contents can't be removed
    std::fs::set_permissions(&blocker, std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(out.status.success(), "prune should succeed overall");

    let lines = parse_ndjson(&out.stdout);
    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();

    // One should fail (prune-err-a), one should succeed (prune-err-b)
    let failed_count = summary["failed"].as_u64().unwrap();
    let deleted_count = summary["deleted"].as_u64().unwrap();
    assert_eq!(failed_count, 1, "one deletion should fail");
    assert_eq!(deleted_count, 1, "one deletion should succeed");

    // Verify the error line exists
    let error_line = lines
        .iter()
        .find(|l| l["type"] == "session" && l["action"] == "error")
        .expect("should have an error action line");
    assert!(
        error_line.get("error").is_some() && !error_line["error"].is_null(),
        "error line should have error message"
    );

    // Cleanup: restore permissions so TempDir can clean up
    std::fs::set_permissions(&blocker, std::fs::Permissions::from_mode(0o755)).unwrap();
}
