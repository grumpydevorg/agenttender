---
name: using-tender
description: Use when working with the `tender` CLI to supervise durable shells, REPLs, or long-running sessions locally or on remote hosts. Covers the split between `--host`-supported commands and local-only ones (`run`, `exec`, `wrap`, `prune`), reliable `tender exec` patterns for DuckDB/Python/shell sessions, log/watch/wait workflows, quoting hazards when `ssh` wrapping remote `exec`, and building/cross-compiling `tender`. DO NOT TRIGGER when editing Tender source code itself (modifying .rs files, working on Tender internals); use the Rust workflow for that. This skill is for using Tender, not developing it.
---

# Using Tender

## Overview

Tender (`tender`) is a session supervisor for long-lived child processes: shells, REPLs, and long-running scripts. `tender exec` becomes a thin client against a persistent session, so shell state (cwd, env, activated venv) or REPL state (imports, loaded tables, attached DuckDB databases) survives across separate tool calls.

Reach for Tender when work must outlive the agent's subprocess boundary: a multi-hour extraction on a remote host, a persistent DuckDB or Python session, a supervised shell that must keep its state, or a batch pipeline that needs `wait`, `log`, `watch`, `kill`, or `--after`.

`tender --help` lists the verbs. The sections below focus on the parts that are easy to miss from help alone.

## Quick-start recipes

### A persistent DuckDB session on a remote host

```bash
# Start the remote session through Tender's SSH transport.
tender --host remote start --stdin ddb -- duckdb /path/to/db.duckdb

# Drive the REPL on the remote host itself. `exec` is local-only today.
ssh remote "tender exec ddb -- \"CREATE TABLE t AS SELECT * FROM read_parquet('x.parquet');\""
ssh remote 'tender exec ddb -- "SELECT count(*) FROM t;"'

# Observe from the local machine with host-supported commands.
tender --host remote log ddb -f
tender --host remote status ddb
```

Key flags for `tender start`:

- `--stdin`: enable the stdin-pipe lane so `exec` can frame commands into the session.
- `--exec-target duckdb` / `python-repl` / `powershell`: force the exec protocol when inference is unclear.
- `--namespace <ns>`: group related sessions so `watch` can follow them together.

### A supervised long-running script

```bash
# Remote script already exists on the target host.
tender --host remote start extract_all -- /path/extract_all.sh

# Native observation commands; no ssh+tail+sleep loop needed.
tender --host remote status extract_all
tender --host remote log extract_all -f
tender --host remote log extract_all --raw | rg "FAIL"
tender --host remote wait extract_all --timeout 3600

# `watch` follows namespaces, not one named session.
tender --host remote watch --namespace nightly --events --logs
```

For local scripts, `tender run` is the convenience layer over `start`:

```bash
tender run --detach ./extract_all.sh
```

Useful flags for batch work:

- `--detach`: return immediately and leave the session running.
- `--after <session>`: wait for one or more sessions to exit before starting.
- `--on-exit <command>`: fire a hook after the child exits.
- `--replace`: kill and restart an existing session of the same name.
- `--timeout <sec>`: kill the child if it overruns.

## Hard-won gotchas

### 1. `--host` is not universal

Tender accepts `--host` as a global flag, but only these commands work over SSH today:

- `start`
- `status`
- `list`
- `log`
- `push`
- `kill`
- `wait`
- `watch`
- `attach`

These are local-only:

- `run`
- `exec`
- `wrap`
- `prune`

That split matters most at the REPL boundary. Start and observe the remote session with `tender --host remote ...`, but drive `exec` on the remote host itself:

```bash
tender --host remote start --stdin py -- python3 -i
ssh remote 'tender exec py -- "print(2 + 2)"'
```

Trivial commands like that are fine inline; for anything multi-line, see the script + scp pattern in §5.

Also note that `watch` does not take a session name. It watches all visible sessions, optionally filtered by `--namespace`.

### 2. `tender exec` takes argv, not a shell snippet

This works:

```bash
tender exec sh -- cd /tmp
tender exec sh -- pwd
```

This does not mean "run two shell statements":

```bash
tender exec sh -- "cd /tmp && pwd"
```

That quoted string becomes one argv element. For multi-step shell work, either use separate `exec` calls or wrap explicitly:

```bash
tender exec sh -- bash -c 'cd /tmp && pwd'
```

For Python REPL or DuckDB sessions the same argv rule applies — the payload is interpreted as Python or SQL by the running REPL, not as shell.

### 3. Error propagation is via `$?` or `.exit_code`, not stdout grep

`tender exec` returns a JSON envelope:

```json
{"session":"ddb","stdout":"…","stderr":"Parser Error: …","exit_code":1,"cwd_after":".","timed_out":false,"truncated":false}
```

Non-zero inner exit codes propagate correctly to the shell's `$?`. Gate success on `$?` or on `.exit_code`. Do not grep stdout for `"error"`.

Shell idioms:

```bash
set -euo pipefail
tender exec ddb -- "CREATE TABLE t AS …" >/dev/null
tender exec ddb -- "SELECT count(*) FROM t;" >/dev/null

tender exec ddb -- "$SQL" | jq -e '.exit_code == 0' >/dev/null \
  || { echo "FAIL"; exit 1; }
```

When debugging a failed exec, inspect `.stderr`. The session log still has the surrounding context after the fact.

### 4. `"annotation too large, dropping"` stderr noise is not the exec failing

Large `tender exec` calls can emit:

```text
tender exec: annotation too large even after truncation, dropping
```

That warning only means the full command text could not fit in the annotation line written to `output.log`. The exec itself may still have succeeded or failed normally. Treat it as log-side noise, not as command failure.

If downstream tooling needs a quiet stream, filter the warning explicitly:

```bash
tender exec ddb -- "$SQL" 2> >(rg -v "annotation too large" >&2)
```

### 5. Nested-quote failure when `ssh` wrapping remote `exec`

`ssh remote 'tender exec ddb -- "<SQL>"'` stacks several quoting layers:

1. outer ssh shell quoting
2. `tender exec` argv quoting
3. SQL or Python string quoting inside the payload

That is where paths or SQL string literals get silently mangled.

Robust pattern:

- write the driver script locally
- copy it to the remote host
- run the script remotely

```bash
scp ~/projects/extract/extract_day.sh remote:/path/
ssh remote 'chmod +x /path/extract_day.sh && /path/extract_day.sh arg1 arg2'
```

Inside the remote script, only the Tender payload quoting remains. That is much easier to reason about than nesting `ssh` and `tender exec` inline.

### 6. Building Tender for target hosts

Tender does not ship binaries. Build from source.

**Linux (native build):**

```bash
cd ~/Documents/projects/tender
cargo build --release
install -m 0755 target/release/tender ~/.local/bin/
```

**macOS → aarch64-linux cross-compile** (for edge hosts):

On macOS targeting `aarch64-unknown-linux-musl`, the simplest working path is `cargo-zigbuild`:

```bash
brew install zig
cargo install cargo-zigbuild
cd ~/Documents/projects/tender
cargo zigbuild --release --target aarch64-unknown-linux-musl

scp target/aarch64-unknown-linux-musl/release/tender remote:~/.local/bin/tender
ssh remote 'chmod +x ~/.local/bin/tender && tender help'
```

Musl + static linking avoids libc-version drift across edge hosts.

**macOS → x86_64 Windows cross-compile** (for testing on a Windows VM):

```bash
rustup target add x86_64-pc-windows-gnu
cd ~/Documents/projects/tender
cargo zigbuild --release --target x86_64-pc-windows-gnu --bin tender

scp target/x86_64-pc-windows-gnu/release/tender.exe win11-vm:.local/bin/tender.exe
ssh win11-vm 'tender --help'
```

Runs natively on Windows ARM64 via x64 emulation; no perf concern for an I/O-bound supervisor.

**Native Windows dev loop** (for running `cargo test` on the VM):

`rust-mingw` ships `dlltool.exe` but not the rest of binutils, so `windows-sys` builds fail with `dlltool: CreateProcess [error]` (it's spawning a missing `as.exe`). Install MSYS2 binutils once:

```powershell
winget install -e --id MSYS2.MSYS2 --silent --accept-source-agreements --accept-package-agreements
& "C:\msys64\usr\bin\bash.exe" -lc "pacman -S --noconfirm mingw-w64-x86_64-binutils"
# Then for each cargo invocation:
$env:PATH = "$env:USERPROFILE\.cargo\bin;C:\msys64\mingw64\bin;$env:PATH"
cargo test --lib --tests
```

Long Windows builds + test runs benefit from being launched under `tender start --replace`: the build survives SSH disconnect, `tender log -f` streams output, `tender wait` blocks for the result.

### 7. One session means one in-flight `exec`

If a driver is already streaming `tender exec` calls into session `ddb`, a second concurrent `exec` against that same session fails with:

```text
another exec is already running on this session
```

That is expected. The session is serialized by an exec lock. Either wait for the active driver to finish or start a second session (`ddb2`, `py2`, `shell2`) for interactive inspection.

## Known limitations worth filing against Tender

- `--host` appears in global help even on local-only commands like `exec`, which invites a failed first attempt.
- `tender log` cannot show the original payload for an oversized dropped annotation; a small breadcrumb with size and hash would help.
- `tender exec` still emits annotation-overflow noise on stderr during large payloads.
- The PowerShell exec target is known-limited compared with POSIX shell, Python REPL, and DuckDB.

## See also

- Architecture overview: `docs/architecture/README.md`
- Transport boundaries: `docs/architecture/06-transport-boundaries.md`

(This skill lives alongside the Tender source at `.claude/skills/using-tender/`. It is installed into `~/.claude/skills/` by symlink via `install.sh` in the same directory.)
