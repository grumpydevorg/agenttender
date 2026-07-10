use std::io;
use std::process::{Command, Stdio};

/// Errors from the SSH transport layer.
#[derive(Debug, thiserror::Error)]
pub enum SshError {
    /// Failed to spawn the ssh process itself.
    #[error("failed to spawn ssh: {0}")]
    SpawnFailed(io::Error),

    /// SSH process exited with a connection-level failure (exit code 255).
    #[error("ssh transport failed: connection or authentication failure (exit 255)")]
    TransportFailed,

    /// SSH process was killed by a signal or returned an unexpected OS error.
    #[error("ssh process terminated abnormally")]
    Abnormal,

    /// The SSH destination is empty or option-shaped (begins with `-`). It is
    /// passed to the local `ssh` as a bare positional argument, so a value like
    /// `-oProxyCommand=<cmd>` would be parsed by the local ssh as an option —
    /// local command execution. Rejected before ssh is ever spawned.
    #[error("invalid --host destination {0:?}: must not be empty or begin with '-'")]
    InvalidDestination(String),
}

/// Commands whose argv is forwarded verbatim over SSH transport.
///
/// `exec` is deliberately absent: it is remote-capable, but travels via
/// the frame transport (`exec_ssh_frame`) — the request rides the SSH
/// stdin channel as one JSON frame, never the remote argv. Local-only
/// commands (`run`, `wrap`, `prune`) are rejected when `--host` is set:
/// `wrap` relies on local process context, `run` reads a local script
/// file, `prune` walks the local session root.
pub const REMOTE_COMMANDS: &[&str] = &[
    "start", "status", "list", "log", "push", "kill", "wait", "watch", "attach",
];

/// Check whether a subcommand is supported for remote execution.
pub fn is_remote_supported(subcommand: &str) -> bool {
    REMOTE_COMMANDS.contains(&subcommand)
}

/// Validate an SSH destination before it is handed to the local `ssh` binary.
///
/// The destination is a *bare* positional argument to `ssh`. An empty or
/// option-shaped value (one beginning with `-`, e.g. `-oProxyCommand=<cmd>`)
/// would be parsed by the local ssh as an *option*, enabling local command
/// execution. Valid destinations — `user@host`, ssh-config aliases, IPv4, and
/// bracketed IPv6 (`[::1]`) — never begin with `-`, so a single non-empty /
/// leading-dash check preserves every legitimate form while closing the vector.
///
/// Enforced at the CLI boundary (exit 2) AND re-checked inside `exec_ssh` /
/// `exec_ssh_frame`, so no non-CLI caller can bypass the guard.
pub fn validate_destination(host: &str) -> Result<(), SshError> {
    if host.is_empty() || host.starts_with('-') {
        return Err(SshError::InvalidDestination(host.to_owned()));
    }
    Ok(())
}

/// Build the SSH command line for remote tender invocation.
///
/// SSH sends everything after the destination as a single string to the
/// remote login shell. To preserve argument boundaries (spaces, quotes,
/// shell metacharacters), each tender argument is individually
/// shell-quoted using `shell_words::quote()` (POSIX quoting).
///
/// The resulting local argv is:
///   ssh -T -o ConnectTimeout=10 <host> tender 'arg1' 'arg2' ...
///
/// SSH concatenates "tender", "'arg1'", "'arg2'" with spaces and sends
/// the result to the remote login shell, which re-splits it into argv.
///
/// No `--` separator is used between the host and the remote command.
/// SSH does not treat `--` as an option terminator — it would be sent
/// literally to the remote shell as part of the command string.
///
/// **Scope:** POSIX remote login shells only (bash, zsh, sh, etc.).
/// Windows remote hosts (PowerShell, cmd.exe) need a different quoting
/// strategy and are deferred to a follow-on slice.
pub fn build_ssh_command(
    host: &str,
    tender_args: &[String],
    allocate_tty: bool,
) -> Result<Command, SshError> {
    // Fail closed: the builder validates the destination, so no caller can
    // obtain a spawnable Command for an option-shaped/empty host — not just the
    // CLI path. See `validate_destination`.
    validate_destination(host)?;
    let mut cmd = Command::new("ssh");
    let tty_flag = if allocate_tty { "-t" } else { "-T" };
    cmd.args([tty_flag, "-o", "ConnectTimeout=10", host]);

    // Each tender arg becomes a separate ssh argv entry, individually
    // quoted for the remote shell. SSH concatenates all args after the
    // host with spaces to form the remote command string.
    cmd.arg("tender");
    for arg in tender_args {
        cmd.arg(shell_words::quote(arg).into_owned());
    }

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    Ok(cmd)
}

/// Execute a remote tender command via SSH, streaming stdout/stderr
/// directly to the local stdout/stderr.
///
/// Returns the remote tender exit code on success.
/// Returns `SshError::TransportFailed` for SSH connection failures (exit 255),
/// or `SshError::InvalidDestination` for an option-shaped/empty host.
pub fn exec_ssh(host: &str, tender_args: &[String], allocate_tty: bool) -> Result<i32, SshError> {
    let mut child = build_ssh_command(host, tender_args, allocate_tty)?
        .spawn()
        .map_err(SshError::SpawnFailed)?;

    let status = child.wait().map_err(SshError::SpawnFailed)?;

    match status.code() {
        Some(255) => Err(SshError::TransportFailed),
        Some(code) => Ok(code),
        None => Err(SshError::Abnormal),
    }
}

/// The remote argv for frame-transport exec — constant by construction:
/// nothing user-controlled rides in argv, so there is no shell-quoting
/// layer to traverse (2026-07-08-remote-exec-host-parity.md slice 2). The frame
/// itself travels on the SSH stdin channel as opaque bytes.
fn build_ssh_exec_frame_command(host: &str, inherit_stdin: bool) -> Result<Command, SshError> {
    validate_destination(host)?;
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-T",
        "-o",
        "ConnectTimeout=10",
        host,
        "tender",
        "exec",
        "--frame-from-stdin",
    ]);
    cmd.stdin(if inherit_stdin {
        Stdio::inherit()
    } else {
        Stdio::piped()
    })
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit());
    Ok(cmd)
}

/// Execute a remote exec by shipping the frame over the SSH stdin
/// channel. `frame` = `None` inherits local stdin (the
/// `--frame-from-stdin` passthrough form — the caller's stdin already
/// is the frame).
///
/// A failed write to the remote's stdin is deliberately ignored: it
/// means the remote died early (EPIPE), and the remote's exit status is
/// the authoritative outcome either way.
///
/// # Errors
/// `SshError::TransportFailed` on ssh exit 255 (which shadows a remote
/// command genuinely exiting 255 — an inherent ssh limitation),
/// `SshError::SpawnFailed`/`Abnormal` as for `exec_ssh`.
pub fn exec_ssh_frame(host: &str, frame: Option<&[u8]>) -> Result<i32, SshError> {
    let mut child = build_ssh_exec_frame_command(host, frame.is_none())?
        .spawn()
        .map_err(SshError::SpawnFailed)?;

    if let Some(bytes) = frame
        && let Some(mut stdin) = child.stdin.take()
    {
        use std::io::Write;
        let _ = stdin.write_all(bytes);
        // Dropping stdin closes the channel — the remote sees EOF.
    }

    let status = child.wait().map_err(SshError::SpawnFailed)?;
    match status.code() {
        Some(255) => Err(SshError::TransportFailed),
        Some(code) => Ok(code),
        None => Err(SshError::Abnormal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame-transport argv is constant: the payload never appears.
    #[test]
    fn exec_frame_argv_is_constant() {
        let cmd = build_ssh_exec_frame_command("user@box", false).unwrap();
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            [
                "-T",
                "-o",
                "ConnectTimeout=10",
                "user@box",
                "tender",
                "exec",
                "--frame-from-stdin"
            ]
        );
    }

    #[test]
    fn validate_destination_rejects_empty_and_option_shaped() {
        for bad in [
            "",
            "-",
            "-t",
            "--foo",
            "-oProxyCommand=calc",
            "-oProxyCommand=x",
        ] {
            assert!(
                validate_destination(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_destination_preserves_valid_forms() {
        for ok in [
            "box",
            "rick-windows",
            "user@host",
            "user@10.0.0.1",
            "10.0.0.1",
            "host.example.com",
            "[::1]",
            "user@[fe80::1]",
        ] {
            assert!(
                validate_destination(ok).is_ok(),
                "expected {ok:?} to be accepted"
            );
        }
    }

    #[test]
    fn builders_fail_closed_on_invalid_destination() {
        // Neither builder can hand out a spawnable Command for an option-shaped
        // or empty host — so no non-CLI caller can bypass the guard.
        let no_args: Vec<String> = vec![];
        assert!(build_ssh_command("-oProxyCommand=x", &no_args, false).is_err());
        assert!(build_ssh_command("", &no_args, false).is_err());
        assert!(build_ssh_exec_frame_command("-oProxyCommand=x", false).is_err());
        assert!(build_ssh_exec_frame_command("", false).is_err());
        // Valid destinations still build.
        assert!(build_ssh_command("user@box", &no_args, false).is_ok());
        assert!(build_ssh_exec_frame_command("user@box", false).is_ok());
    }

    /// Helper: extract the args that `build_ssh_command` would pass to the ssh binary.
    fn ssh_argv(host: &str, tender_args: &[&str]) -> Vec<String> {
        let args: Vec<String> = tender_args.iter().map(|s| s.to_string()).collect();
        let cmd = build_ssh_command(host, &args, false).unwrap();
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn simple_command() {
        let args = ssh_argv("user@box", &["status", "my-session"]);
        assert_eq!(args[0], "-T");
        assert_eq!(args[1], "-o");
        assert_eq!(args[2], "ConnectTimeout=10");
        assert_eq!(args[3], "user@box");
        assert_eq!(args[4], "tender");
        assert_eq!(args[5], "status");
        assert_eq!(args[6], "my-session");
        assert!(
            !args.contains(&"--".to_string()),
            "no -- in ssh argv: {args:?}"
        );
    }

    #[test]
    fn args_with_spaces_are_quoted() {
        let args: Vec<String> = vec![
            "start".to_string(),
            "job".to_string(),
            "--".to_string(),
            "echo".to_string(),
            "hello world".to_string(),
        ];
        let cmd = build_ssh_command("user@box", &args, false).unwrap();
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let remote_args = &argv[4..];
        let remote_cmd = remote_args.join(" ");
        let parsed = shell_words::split(&remote_cmd).unwrap();
        assert_eq!(
            parsed,
            vec!["tender", "start", "job", "--", "echo", "hello world"]
        );
    }

    #[test]
    fn args_with_shell_metacharacters_are_quoted() {
        let args: Vec<String> = vec![
            "start".to_string(),
            "job".to_string(),
            "--".to_string(),
            "bash".to_string(),
            "-c".to_string(),
            "echo $HOME && ls".to_string(),
        ];
        let cmd = build_ssh_command("user@box", &args, false).unwrap();
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let remote_args = &argv[4..];
        let remote_cmd = remote_args.join(" ");
        let parsed = shell_words::split(&remote_cmd).unwrap();
        assert_eq!(parsed[6], "echo $HOME && ls");
    }

    #[test]
    fn remote_allowlist() {
        assert!(is_remote_supported("start"));
        assert!(is_remote_supported("status"));
        assert!(is_remote_supported("list"));
        assert!(is_remote_supported("log"));
        assert!(is_remote_supported("push"));
        assert!(is_remote_supported("kill"));
        assert!(is_remote_supported("wait"));
        assert!(is_remote_supported("watch"));
        assert!(is_remote_supported("attach"));

        assert!(!is_remote_supported("run"));
        assert!(!is_remote_supported("exec"));
        assert!(!is_remote_supported("wrap"));
        assert!(!is_remote_supported("prune"));
        assert!(!is_remote_supported("_sidecar"));
    }
}
