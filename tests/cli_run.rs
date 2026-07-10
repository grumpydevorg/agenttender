mod harness;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn tender(root: &TempDir) -> Command {
    harness::tender(root)
}

/// Write a Python script (cross-platform, .py extension triggers launcher).
fn write_py_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let content = format!("import sys\n{body}\n");
    std::fs::write(&path, content).unwrap();
    path
}

/// Write a bash script with +x (Unix only).
#[cfg(unix)]
fn write_bash_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let content = format!("#!/bin/bash\n{body}\n");
    std::fs::write(&path, content).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

// ── Core behavior (cross-platform, Python scripts) ─────────────────────

#[test]
fn run_blocks_and_returns_exit_code_zero() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(root.path(), "hello.py", "print('hello-from-run')");

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello-from-run"));
}

#[test]
fn run_propagates_nonzero_exit_code() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(root.path(), "fail.py", "print('failing')\nsys.exit(7)");

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .assert()
        .code(7)
        .stdout(predicate::str::contains("failing"));
}

#[test]
fn run_passes_script_arguments() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(
        root.path(),
        "args.py",
        "print('args:', ' '.join(sys.argv[1:]))",
    );

    tender(&root)
        .args(["run", script.to_str().unwrap(), "foo", "bar"])
        .assert()
        .success()
        .stdout(predicate::str::contains("args: foo bar"));
}

#[test]
fn run_replace_reruns_script() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(root.path(), "rerun.py", "print('rerun-output')");

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .assert()
        .success();

    tender(&root)
        .args(["run", "--replace", script.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("rerun-output"));
}

#[test]
fn run_foreground_overrides_detach_directive() {
    let root = TempDir::new().unwrap();
    // Python uses # for comments, so #tender: directives work natively.
    let script = write_py_script(
        root.path(),
        "detachable.py",
        "#tender: detach\nprint('foreground-output')",
    );

    // Without --foreground, the directive causes detach (JSON output).
    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .timeout(std::time::Duration::from_secs(5))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\": \"Running\""));

    // With --foreground, the directive is overridden.
    tender(&root)
        .args(["run", "--foreground", "--replace", script.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("foreground-output"));
}

#[test]
fn run_detach_returns_immediately_with_json() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(root.path(), "slow.py", "import time; time.sleep(30)");

    tender(&root)
        .args(["run", "--detach", script.to_str().unwrap()])
        .timeout(std::time::Duration::from_secs(5))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\": \"Running\""));
}

#[test]
fn run_session_name_from_filename() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(
        root.path(),
        "my-build.py",
        "#tender: detach\nimport time; time.sleep(30)",
    );

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .timeout(std::time::Duration::from_secs(5))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"session\": \"my-build\""));
}

#[test]
fn run_session_directive_overrides_filename() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(
        root.path(),
        "build.py",
        "#tender: session=custom-name\n#tender: detach\nimport time; time.sleep(30)",
    );

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .timeout(std::time::Duration::from_secs(5))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"session\": \"custom-name\""));
}

#[test]
fn run_directives_map_to_launch_spec() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(
        root.path(),
        "directives.py",
        "#tender: namespace=test-ns\n#tender: timeout=999\n#tender: session=my-session\n#tender: detach\nimport time; time.sleep(30)",
    );

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .timeout(std::time::Duration::from_secs(5))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"session\": \"my-session\""))
        .stdout(predicate::str::contains("\"timeout_s\": 999"))
        .stdout(predicate::str::contains("\"namespace\": \"test-ns\""));
}

#[test]
fn run_cli_flags_override_directives() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(
        root.path(),
        "override.py",
        "#tender: namespace=directive-ns\n#tender: timeout=999\n#tender: detach\nimport time; time.sleep(30)",
    );

    tender(&root)
        .args([
            "run",
            "--namespace",
            "cli-ns",
            "--timeout",
            "42",
            script.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(5))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"namespace\": \"cli-ns\""))
        .stdout(predicate::str::contains("\"timeout_s\": 42"));
}

// ── --shell override ────────────────────────────────────────────────────

#[test]
fn run_shell_flag_uses_specified_interpreter() {
    let root = TempDir::new().unwrap();
    // Write a plain text file with Python code — no mapped extension.
    let script_path = root.path().join("noshebang.txt");
    std::fs::write(&script_path, "print('shell-flag-works')\n").unwrap();

    // Use --shell to provide the interpreter explicitly.
    let py = if cfg!(windows) { "py" } else { "python3" };
    tender(&root)
        .args(["run", "--shell", py, script_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("shell-flag-works"));
}

// ── Extension mapping ───────────────────────────────────────────────────

#[test]
fn run_py_extension_uses_python() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(root.path(), "check.py", "print('py-ext-works')");

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("py-ext-works"));
}

// ── Error paths ─────────────────────────────────────────────────────────

#[test]
fn run_nonexistent_script_fails() {
    let root = TempDir::new().unwrap();

    tender(&root)
        .args(["run", "/nonexistent/path/script.sh"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("script not found"));
}

#[test]
fn run_unknown_extension_fails_with_hint() {
    let root = TempDir::new().unwrap();
    let script_path = root.path().join("mystery.xyz");
    std::fs::write(&script_path, "some content\n").unwrap();

    tender(&root)
        .args(["run", script_path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot determine interpreter"))
        .stderr(predicate::str::contains("--shell"));
}

#[test]
fn run_invalid_cli_namespace_fails() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(root.path(), "good.py", "print('hi')");

    tender(&root)
        .args(["run", "--namespace", "bad name", script.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("whitespace"));
}

#[test]
fn run_invalid_namespace_directive_fails() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(
        root.path(),
        "badns.py",
        "#tender: namespace=bad name\nprint('hi')",
    );

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid namespace"));
}

#[test]
fn run_invalid_session_directive_fails() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(
        root.path(),
        "badsession.py",
        "#tender: session=my.bad.name\nprint('hi')",
    );

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid session name"));
}

#[test]
fn run_unknown_directive_errors() {
    let root = TempDir::new().unwrap();
    let script = write_py_script(root.path(), "bad.py", "#tender: timout=30\nprint('hi')");

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown directive"));
}

// ── ExecTarget::None ────────────────────────────────────────────────────

#[test]
fn run_session_rejects_exec() {
    let _lock = lock();
    let root = TempDir::new().unwrap();

    let script = write_py_script(
        root.path(),
        "server.py",
        "#tender: detach\nimport time; time.sleep(60)",
    );

    tender(&root)
        .args(["run", "--stdin", script.to_str().unwrap()])
        .timeout(std::time::Duration::from_secs(5))
        .assert()
        .success();

    harness::wait_running(&root, "server");

    tender(&root)
        .args(["exec", "server", "--", "echo", "hello"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no exec target"));

    let _ = tender(&root).args(["kill", "server", "--force"]).assert();
}

// ── Bash-specific (Unix only) ───────────────────────────────────────────

#[cfg(unix)]
#[test]
fn run_bash_sh_extension() {
    let root = TempDir::new().unwrap();
    let script = write_bash_script(root.path(), "test.sh", "echo bash-ext-works");

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("bash-ext-works"));
}

#[cfg(unix)]
#[test]
fn run_shebang_fallback_for_extensionless() {
    let root = TempDir::new().unwrap();
    // Extensionless file with shebang, not executable
    let script_path = root.path().join("myscript");
    std::fs::write(&script_path, "#!/bin/bash\necho shebang-fallback\n").unwrap();

    tender(&root)
        .args(["run", script_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("shebang-fallback"));
}

#[cfg(unix)]
#[test]
fn run_executable_bit_direct() {
    let root = TempDir::new().unwrap();
    let script = write_bash_script(root.path(), "direct", "echo direct-exec");

    tender(&root)
        .args(["run", script.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("direct-exec"));
}
