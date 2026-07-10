#[allow(unused_imports)]
use assert_cmd::Command;
#[allow(unused_imports)]
use std::path::Path;
#[allow(unused_imports)]
use tempfile::TempDir;

/// Create a `tender` command rooted in a temp HOME.
#[allow(dead_code)]
pub fn tender(root: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("tender").expect("tender binary not found");
    cmd.env("HOME", root.path());
    // On Windows, ensure Git-for-Windows coreutils (echo, sleep, true, cat)
    // are on PATH so tests can spawn Unix-style commands.
    #[cfg(windows)]
    {
        let git_usr_bin = std::path::Path::new(r"C:\Program Files\Git\usr\bin");
        if git_usr_bin.exists() {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{};{path}", git_usr_bin.display()));
        }
    }
    cmd
}

/// Path to the `test_callback` fixture binary (built by cargo as a sibling of the test binary).
#[allow(dead_code)]
pub fn test_callback_bin() -> String {
    let bin = assert_cmd::cargo::cargo_bin("test_callback");
    bin.to_str()
        .expect("test_callback path is valid UTF-8")
        .to_owned()
}

/// `test_callback_bin()` quoted for embedding in on-exit command strings
/// that will be parsed by `shell_words::split`.
#[allow(dead_code)]
fn test_callback_bin_quoted() -> String {
    shell_words::quote(&test_callback_bin()).into_owned()
}

/// Return an on-exit command string that creates `path` as an empty marker file.
/// Parsed by `shell_words::split` in the sidecar, then executed directly — no shell involved.
#[allow(dead_code)]
pub fn touch_cmd(path: &Path) -> String {
    let quoted = shell_words::quote(path.to_str().expect("path is valid UTF-8"));
    format!("{} touch {quoted}", test_callback_bin_quoted())
}

/// Return an on-exit command string that writes TENDER_SESSION, TENDER_NAMESPACE,
/// and TENDER_EXIT_REASON to the given file.
/// Parsed by `shell_words::split` in the sidecar, then executed directly — no shell involved.
#[allow(dead_code)]
pub fn echo_env_cmd(path: &Path) -> String {
    let quoted = shell_words::quote(path.to_str().expect("path is valid UTF-8"));
    format!("{} echo-env {quoted}", test_callback_bin_quoted())
}

/// Read all stored events for a default-namespace session, merged by
/// (ts, writer, seq) — the spec §4 merge rule.
#[allow(dead_code)]
pub fn read_events(root: &TempDir, session: &str) -> Vec<serde_json::Value> {
    let events_dir = root
        .path()
        .join(format!(".tender/sessions/default/{session}/events"));
    let mut segments: Vec<_> = std::fs::read_dir(&events_dir)
        .expect("events dir exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segments.sort();

    let mut events: Vec<serde_json::Value> = Vec::new();
    for seg in segments {
        for line in std::fs::read_to_string(&seg).unwrap().lines() {
            if !line.is_empty() {
                events.push(serde_json::from_str(line).expect("event line parses"));
            }
        }
    }
    events.sort_by_key(|e| {
        (
            e["ts"].as_str().unwrap().to_owned(),
            e["writer"].as_str().unwrap().to_owned(),
            e["seq"].as_u64().unwrap(),
        )
    });
    events
}

/// Wait for meta.json to show Running state on disk.
#[allow(dead_code)]
pub fn wait_running(root: &TempDir, session: &str) {
    let path = root
        .path()
        .join(format!(".tender/sessions/default/{session}/meta.json"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
                if meta["status"].as_str() == Some("Running") {
                    return;
                }
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for Running state in {session}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Wait for meta.json to reach any terminal state on disk.
#[allow(dead_code)]
pub fn wait_terminal(root: &TempDir, session: &str) -> serde_json::Value {
    let path = root
        .path()
        .join(format!(".tender/sessions/default/{session}/meta.json"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
                let status = meta["status"].as_str().unwrap_or("");
                if status != "Starting" && status != "Running" {
                    return meta;
                }
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for terminal state in {session}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Gate a test on the external `duckdb` CLI. Returns `true` if the test should
/// proceed. When duckdb is absent:
///   - panics if `TENDER_REQUIRE_DUCKDB_TESTS` is set (CI installs duckdb, so a
///     broken install must fail loudly rather than silently pass as green);
///   - otherwise prints a skip notice and returns `false` so the caller returns
///     early (friendly local runs without duckdb installed).
#[allow(dead_code)]
pub fn duckdb_or_skip() -> bool {
    let available = std::process::Command::new("duckdb")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !available {
        if std::env::var_os("TENDER_REQUIRE_DUCKDB_TESTS").is_some() {
            panic!(
                "duckdb not found on PATH but TENDER_REQUIRE_DUCKDB_TESTS is set \
                 -- CI must install the DuckDB CLI"
            );
        }
        eprintln!("skipped: duckdb not on PATH (set TENDER_REQUIRE_DUCKDB_TESTS to enforce)");
    }
    available
}
