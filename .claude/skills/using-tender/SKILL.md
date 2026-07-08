---
name: using-tender
description: Use when working with the `tender` CLI to supervise durable shells, REPLs, or long-running sessions locally or on remote hosts. Covers remote use via `--host` (including remote `exec` over the frame transport), the local-only trio (`run`, `wrap`, `prune`), reliable `tender exec` patterns for DuckDB/Python/shell sessions, log/watch/wait workflows, and building/cross-compiling `tender`. DO NOT TRIGGER when editing Tender source code itself (modifying .rs files, working on Tender internals); use the Rust workflow for that. This skill is for using Tender, not developing it.
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

# Drive and observe it from the local machine. Remote exec ships the
# payload as one JSON frame over the ssh stdin channel — it never
# traverses a shell, so no nested-quoting layer exists.
tender --host remote exec ddb -- "CREATE TABLE t AS SELECT * FROM read_parquet('x.parquet');"
tender --host remote exec ddb -- "SELECT count(*) FROM t;"
tender --host remote log ddb -f
tender --host remote status ddb
```

Key flags for `tender start`:

- `--stdin`: enable the stdin-pipe lane so `exec` can frame commands into the session.
- `--exec-target duckdb` / `python-repl` / `powershell`: force the exec protocol when inference is unclear.
- `--namespace <ns>`: group related sessions so `watch` can follow them together.

### A persistent PowerShell session on a Windows host

```bash
# Start the remote session. The exec target is auto-inferred from `powershell` / `pwsh`.
ssh win11-vm 'tender start --stdin --replace ps -- powershell -NoProfile'

# Drive it with --host: only local bash quoting remains — the payload
# rides the ssh stdin channel, so the Windows shell never parses it.
tender --host win11-vm exec ps -- 'Get-Date -Format yyyy-MM-dd'
tender --host win11-vm exec ps -- 'Set-Location C:\Windows; Get-Location'
tender --host win11-vm exec ps -- '$global:counter = 1; $global:counter += 5; $global:counter'
```

The result envelope is the same as DuckDB / Python REPL: clean `stdout`, partitioned `stderr`, `exit_code`, `cwd_after`. In-session behavior verified on Windows 11 ARM64 against both Windows PowerShell 5.1 (`powershell`) and PowerShell 7+ (`pwsh`); the `--host` frame transport is POSIX-verified — if a Windows host misbehaves, fall back to `ssh win11-vm 'tender exec ps -- "…"'` and see gotcha §7. See gotcha §6 for the `$global:` scope rule that bites every PowerShell user once.

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

### 1. Three verbs are local-only — and they fail loudly now

`exec` works over `--host` (frame transport). `run`, `wrap`, and `prune` are local-only, and saying `--host` on them exits 2 with a pre-filled fallback:

```text
$ tender --host remote run deploy.sh
error: 'run' is local-only and does not support --host
try:  ssh remote 'tender run deploy.sh'
```

Paste the printed `try:` line — it reconstructs your exact command for the remote host. (`wrap` additionally needs the supervised-process env, so it only makes sense inside a tender-supervised process on that host.)

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

For Python REPL or DuckDB sessions the same argv rule applies — the payload is interpreted as Python or SQL by the running REPL, not as shell. The same rule holds with `--host`: the frame carries argv, not a shell string.

### 3. Error propagation is via `$?` or `.exit_code`, not stdout grep

`tender exec` returns a JSON envelope:

```json
{"session":"ddb","stdout":"…","stderr":"Parser Error: …","exit_code":1,"cwd_after":".","timed_out":false,"truncated":false}
```

Non-zero inner exit codes propagate correctly to the shell's `$?` — including through `--host` (ssh forwards the remote exit code; the one collision is 255, which ssh reserves for transport failure). Gate success on `$?` or on `.exit_code`. Do not grep stdout for `"error"`.

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

Large `tender exec` payloads can emit `tender exec: annotation too large even after truncation, dropping` on stderr. It only means the command text could not fit in the annotation line written to `output.log` — the exec itself succeeded or failed normally. Treat it as log-side noise, or filter it: `2> >(rg -v "annotation too large" >&2)`.

### 5. Building Tender for target hosts

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

### 6. PowerShell exec frames have their own variable scope

Each `tender exec` call against a PowerShell session is wrapped in `[scriptblock]::Create($code).Invoke()` so the framing protocol can capture stdout/stderr cleanly. A side-effect: variables set in one frame **do not** persist to the next unless explicitly written to the global scope.

This does not persist:

```bash
tender --host win11-vm exec ps -- '$x = 42'
tender --host win11-vm exec ps -- '$x'   # → empty
```

This does:

```bash
tender --host win11-vm exec ps -- '$global:x = 42'
tender --host win11-vm exec ps -- '$global:x'   # → 42
```

`Set-Variable -Scope Global` works too. cwd is special-cased — `Set-Location` persists automatically and is reflected in `cwd_after`. PowerShell modules, dot-sourced functions, and `Import-Module` state also persist (they're loaded into the session, not the scriptblock scope).

DuckDB and Python REPL sessions don't have this issue — their bindings already live at module/session scope.

### 7. SSH default shell on Windows only matters for ssh-wrapped commands now

With `tender --host`, the remote argv is constant (`tender exec --frame-from-stdin` — quote-stable under both `cmd.exe` and `pwsh`) and the payload rides the stdin channel as opaque bytes, so the Windows default-shell quoting rules stop applying to exec payloads entirely.

They still apply when you ssh-wrap other tender commands yourself. Two of rick's Windows hosts differ: `win11-vm` (Parallels) defaults to `cmd.exe` — single quotes are literal, only double quotes group; `rick-windows` (physical) defaults to PowerShell 7 — `$x` expands on the receiving side before tender sees it. For `cmd.exe` hosts, single-quote the bash side and double-quote the cmd side; for PowerShell-default hosts, use `powershell -NoProfile -Command` explicitly.

### 8. PowerShell `Format-*` cmdlets fail inside the exec frame

`Format-Table`, `Format-List`, and friends throw `NullReferenceException: Object reference not set to an instance of an object` when invoked inside a `tender exec` payload:

```bash
tender --host win11-vm exec ps -- 'Get-Process | Select-Object -First 3 | Format-Table'
# → exit_code: 1, stderr: "Object reference not set to an instance of an object."
```

Cause: `Format-*` cmdlets emit `FormatStartData`/`FormatEntryData` records meant for an interactive PowerShell host. Inside the `[scriptblock]::Create($code).Invoke()` wrapper there is no host UI, and the formatter null-derefs.

Use a serialization cmdlet instead — they don't depend on the host:

```bash
tender --host win11-vm exec ps -- 'Get-Process | Select-Object -First 3 Name,Id,WS | ConvertTo-Json -Compress' \
  | jq -r .stdout | jq
```

`ConvertTo-Json`, `ConvertTo-Csv`, `Out-String -Stream`, and writing scalar values directly all work. Pipe through `jq` on the calling side for pretty-printing.

### 9. One session means one in-flight `exec`

If a driver is already streaming `tender exec` calls into session `ddb`, a second concurrent `exec` against that same session fails with:

```text
another exec is already running on this session
```

That is expected. The session is serialized by an exec lock — locally and over `--host` alike. Either wait for the active driver to finish or start a second session (`ddb2`, `py2`, `shell2`) for interactive inspection.

## Scripting: `exec --frame-from-stdin`

The `--host` transport is built on a flag that is independently useful locally: the whole exec request as one versioned JSON frame on stdin, so multi-line SQL/Python payloads never fight argv quoting.

```bash
# A multi-line SQL file as the payload — newlines and quotes intact,
# because the frame carries argv as JSON, not a shell string.
jq -cn --rawfile sql query.sql \
  '{v: 1, session: "ddb", cmd: [$sql], timeout: 300}' \
  | tender exec --frame-from-stdin
```

Frame schema (v1): `{"v":1,"session":"<name>","namespace":"<ns>"?,"cmd":["argv",…],"timeout":<secs>?}`. A malformed or wrong-version frame exits 2 before any side effect. `tender --host h exec --frame-from-stdin` forwards your local stdin frame to the remote verbatim.

## Known limitations worth filing against Tender

- `tender log` cannot show the original payload for an oversized dropped annotation; a small breadcrumb with size and hash would help.
- `tender exec` still emits annotation-overflow noise on stderr during large payloads (gotcha §4).
- PowerShell exec scope rule (gotcha §6) is correct behavior but surprises every new user. A `--persist-scope` flag or session-level toggle would help.
- `--host exec` against a Windows remote is untested on real hardware (the frame transport is platform-neutral and POSIX-verified; a `win11-vm` smoke run would close this).

## See also

- Architecture overview: `docs/architecture/README.md`
- Transport boundaries: `docs/architecture/06-transport-boundaries.md`

(This skill lives alongside the Tender source at `.claude/skills/using-tender/`. It is installed into `~/.claude/skills/` by symlink via `install.sh` in the same directory.)
