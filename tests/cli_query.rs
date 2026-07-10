//! `tender query` — event-log analytics v1 (DuckDB over the JSONL event log).
//!
//! Analytical surface: locate the event segments in scope, register an `events`
//! view over them (ts as TIMESTAMP, data as JSON), run the user's SQL via the
//! external `duckdb` CLI. Read-only; no new write path.

mod harness;

use harness::tender;
use predicates::prelude::*;
use tempfile::TempDir;

/// Write `lines` as one JSONL segment under `<ns>/<session>/events/`.
fn seed(root: &TempDir, ns: &str, session: &str, lines: &[&str]) {
    let dir = root
        .path()
        .join(format!(".tender/sessions/{ns}/{session}/events"));
    std::fs::create_dir_all(&dir).unwrap();
    // Any *.jsonl name is a valid segment; the view discovers by extension.
    let seg = dir.join("00000000-0000-7000-8000-000000000001.jsonl");
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(&seg, body).unwrap();
}

/// Two exec.result events (exit 0 and 7) in default/s1, one hook event in agents/s2.
fn seed_mixed(root: &TempDir) {
    seed(
        root,
        "default",
        "s1",
        &[
            r#"{"v":1,"id":"a1","ts":"2026-07-09T10:00:00.000000Z","kind":"exec.result","namespace":"default","session":"s1","source":"tender.exec","data":{"exit_code":0,"command":"echo hi"}}"#,
            r#"{"v":1,"id":"a2","ts":"2026-07-09T10:00:01.000000Z","kind":"exec.result","namespace":"default","session":"s1","source":"tender.exec","data":{"exit_code":7,"command":"false"}}"#,
        ],
    );
    seed(
        root,
        "agents",
        "s2",
        &[
            r#"{"v":1,"id":"a3","ts":"2026-07-09T11:00:00.000000Z","kind":"hook.post_tool_use","namespace":"agents","session":"s2","source":"claude.hook","data":{"tool":"Bash"}}"#,
        ],
    );
}

#[test]
fn query_counts_all_events_across_namespaces() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    tender(&root)
        .args(["query", "SELECT COUNT(*) AS n FROM events"])
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));
}

#[test]
fn query_group_by_kind_reads_kind_column() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    tender(&root)
        .args([
            "query",
            "SELECT kind, COUNT(*) AS n FROM events GROUP BY kind ORDER BY kind",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("exec.result"))
        .stdout(predicate::str::contains("hook.post_tool_use"));
}

#[test]
fn query_data_is_json_and_ts_is_timestamp() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    // data->>'exit_code' proves `data` is JSON; the ts comparison proves ts is a
    // real TIMESTAMP. Exactly one exec.result has a non-zero exit code.
    tender(&root)
        .args([
            "query",
            "SELECT COUNT(*) FILTER (WHERE (data->>'exit_code')::INT != 0) AS failures \
             FROM events \
             WHERE kind = 'exec.result' AND ts > TIMESTAMP '2026-07-09 09:00:00'",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("1"));
}

#[test]
fn query_namespace_scopes_the_view() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    // Only the agents namespace is in scope: default/s1's events are excluded.
    tender(&root)
        .args([
            "query",
            "--namespace",
            "agents",
            "SELECT DISTINCT namespace FROM events",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("agents"))
        .stdout(predicate::str::contains("default").not());
}

#[test]
fn query_from_file_runs_sql_from_disk() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    let sql_path = root.path().join("q.sql");
    std::fs::write(&sql_path, "SELECT COUNT(*) AS n FROM events;\n").unwrap();

    tender(&root)
        .args(["query", "--file"])
        .arg(&sql_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));
}

#[test]
fn query_empty_scope_returns_zero() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    // No sessions seeded at all: the view exists but is empty.

    tender(&root)
        .args(["query", "SELECT COUNT(*) AS n FROM events"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0"));
}

#[test]
fn query_bad_sql_propagates_failure() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    tender(&root)
        .args(["query", "SELECT * FROM does_not_exist"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does_not_exist"));
}

#[test]
fn query_missing_duckdb_errors_clearly() {
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    // Empty PATH: the `duckdb` binary cannot be found. The error must name the
    // tool and how to fix it, not leak a bare "No such file or directory".
    tender(&root)
        .env("PATH", "")
        .args(["query", "SELECT COUNT(*) FROM events"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("duckdb"))
        .stderr(predicate::str::contains("PATH"));
}

#[test]
fn query_version_reports_duckdb() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["query", "--version"])
        .assert()
        .success()
        .stdout(predicate::str::contains("DuckDB"));
}

#[test]
fn query_requires_sql_file_or_shell() {
    let root = TempDir::new().unwrap();

    // Bare `tender query` with no SQL, no --file, no --shell, no --version is a
    // usage error.
    tender(&root).args(["query"]).assert().failure();
}

#[test]
fn query_tolerates_a_malformed_line() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    // A corrupt/torn line between two valid events must not kill the query: it
    // is skipped, and the two real events still count. (Unparseable JSON is not
    // rescued by TRY_CAST — the reader itself must tolerate it.)
    seed(
        &root,
        "default",
        "s1",
        &[
            r#"{"v":1,"id":"a1","ts":"2026-07-09T10:00:00.000000Z","kind":"exec.result","namespace":"default","session":"s1","data":{"exit_code":0}}"#,
            "not-json{",
            r#"{"v":1,"id":"a2","ts":"2026-07-09T10:00:01.000000Z","kind":"exec.result","namespace":"default","session":"s1","data":{"exit_code":7}}"#,
        ],
    );

    tender(&root)
        .args(["query", "SELECT COUNT(*) AS n FROM events"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2"));
}
