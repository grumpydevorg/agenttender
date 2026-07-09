---
id: powershell-exec-side-channel
depends_on:
  - powershell-exec-framing
links:
  - 2026-05-10-powershell-exec-framing.md
  - ../../../src/exec_frame.rs
  - ../../../src/commands/exec.rs
---

# PowerShell Exec — Side-Channel Result Transport

**Status: implemented.** See "Implementation Notes" below for the two empirical findings that diverged from the original design.

Move PowerShell exec result reporting off the captured stdout stream and onto a side-channel result file (the existing `PythonRepl` pattern) so user-visible stdout is clean — no prompt prefix, no echoed framing line, no PowerShell terminal escapes.

## Why

`fix(exec): PowerShell frame supports arbitrary expressions` (commit `3f6ff2b`) addressed the *framing* half of `powershell-exec-framing.md`: arbitrary expressions, pipelines, multi-statement, and across-call state now work. Three failure modes from that plan are gone.

The *transcript-noise* half is unchanged. A real `tender exec ps -- echo hello-world` against a `powershell -NoProfile` session captures stdout as:

```
PS C:\Users\rick> $LASTEXITCODE = $null; echo hello-world; $__tender_s = ... ; Write-Output ('__TENDER_EXEC__ ...')
hello-world
```

That contains: prompt + echoed framing line + actual output. The sentinel parser already extracts exit code and cwd correctly, but the user-facing `stdout` field in the exec result envelope is polluted. The plan called out two options for fixing this; this scope picks the second:

> If clean sentinel-in-stdout framing proves too fragile, prefer a side-channel result file similar to `PythonRepl`.

Empirically prompt-suppression is too fragile (PowerShell's input echo on pipe-stdin is a host-internal behavior; prompt override fixes only half of it). The side-channel path is the robust answer.

## Goal

`tender exec ps -- '$x = 1; $x + 1'` returns:

```json
{
  "session": "ps",
  "stdout": "2",
  "stderr": "",
  "exit_code": 0,
  "cwd_after": "C:\\Users\\rick",
  ...
}
```

with `stdout` containing **only** the user code's output. No prompt, no framing transcript, no ANSI noise.

`stderr` contains only the user code's stderr. `cwd_after` reflects any `Set-Location` performed by the user code.

## Design Direction

Mirror the `PythonRepl` design end-to-end. The shape is well-tested and the existing infrastructure handles polling, timeout, and drain.

### 1. Payload encoding

The user's PowerShell payload + result path are base64-encoded by tender (Rust side) and decoded by PowerShell (frame side). Same trick as `python_frame` — avoids quoting collisions on Windows paths (`\t`, `\U`, spaces) and arbitrary user code containing single quotes, here-strings, etc.

```rust
let encoded_code = base64::engine::general_purpose::STANDARD.encode(code);
let encoded_path = base64::engine::general_purpose::STANDARD.encode(result_path);
```

### 2. Frame shape (one logical line, sent via stdin)

PowerShell-side decode + execute + capture + atomic write:

```powershell
$_b = [Convert]::FromBase64String('<encoded_code>')
$_code = [System.Text.Encoding]::UTF8.GetString($_b)
$_rp = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('<encoded_path>'))
$_tmp = "$_rp.tmp"
$_outBuf = New-Object System.Text.StringBuilder
$_errBuf = New-Object System.Text.StringBuilder
$_exit = 0
try {
  & ([scriptblock]::Create($_code)) `
    2>&1 `
    | ForEach-Object {
        if ($_ -is [System.Management.Automation.ErrorRecord]) {
          [void]$_errBuf.AppendLine($_.ToString())
        } else {
          [void]$_outBuf.AppendLine(($_ | Out-String -Stream))
        }
      }
  $_exit = if ($null -ne $LASTEXITCODE) { $LASTEXITCODE } elseif ($?) { 0 } else { 1 }
} catch {
  [void]$_errBuf.AppendLine($_.Exception.Message)
  $_exit = 1
}
$_payload = @{
  exit_code = $_exit
  cwd       = (Get-Location).Path
  stdout    = $_outBuf.ToString()
  stderr    = $_errBuf.ToString()
} | ConvertTo-Json -Compress
[System.IO.File]::WriteAllText($_tmp, $_payload)
[System.IO.File]::Move($_tmp, $_rp)
```

Notes:
- `[scriptblock]::Create($_code).Invoke()` is what makes arbitrary user expressions execute — same script-block pattern the framing fix already uses inline.
- `2>&1 | ForEach-Object` partitions stream-6 ErrorRecord objects from real stdout. PowerShell's "stderr" semantics differ from POSIX: it's a typed object stream, so we discriminate by type rather than by file descriptor.
- `[System.IO.File]::Move` is atomic on the same volume and replaces an existing file when called via the `MoveFileEx`-backed overload (use `[System.IO.File]::Move($src, $dst, $true)` for the overwrite variant if needed).
- Result file location: `{session_dir}/exec-results/{token}.json` — same convention as `PythonRepl`.
- No sentinel needed in the captured stdout. The session log will still receive whatever PowerShell's REPL host prints (prompt, echoed input). That noise is now cosmetic in the session log and never reaches the exec result envelope.

### 3. Tender-side wiring

Almost entirely additive. Reuse `WaitMode::SideChannel`, `wait_side_channel_result`, `drain_until_side_channel`, `SideChannelResult`. Only changes:

- `exec_frame::powershell_frame` signature changes from `(argv, token) -> String` to `(code, result_path) -> String` — parallel to `python_frame`.
- `commands/exec.rs::run_exec` `match` arm for `ExecTarget::PowerShell` switches to the side-channel branch (mkdir `exec-results/`, build `{token}.json` path, frame with that path, return `WaitMode::SideChannel`).
- `commands/exec.rs::cmd_exec` timeout-handling `match` arm switches `PowerShell` from `drain_until_sentinel` to `drain_until_side_channel`.
- `SideChannelResult` field set already covers what we need: `exit_code`, `cwd`, `stdout`, `stderr` (the optional `traceback` is Python-specific; left unused).

### 4. Behavior on session death

If the PowerShell child dies before writing the result file, `wait_side_channel_result` already detects this (polls meta + result-file existence) and returns the appropriate error envelope. No new logic.

## Trade-offs

| | Sentinel-in-stdout (today) | Side-channel (proposed) |
|---|---|---|
| Stdout cleanliness | polluted by prompt + echoed frame | clean |
| Stderr separation | mixed in session log | properly separated in result envelope |
| Reliance on PS host quirks | fragile (PSReadLine, prompt, host-mode) | none |
| Filesystem side-effects per exec | none (log line) | one `{token}.json` write + delete |
| Implementation complexity | small frame string | base64 encode + JSON write + file polling |
| Parallel exec safety | exec-lock already serializes | unchanged |
| Error reporting on script-block parse failure | shows in stderr | shows in stderr (catch path) |

## Implementation Tasks

1. **Add side-channel results dir lifecycle for PowerShell** — already exists for Python; just reuse the `{session}/exec-results/` directory. No new config.

2. **Rewrite `exec_frame::powershell_frame`**
   - Signature: `pub fn powershell_frame(code: &str, result_path: &str) -> String`
   - Body: base64-encode both inputs; emit the single-line PowerShell frame above.
   - Drop the now-unused argv-joining helper (the join logic moves into the caller).

3. **Update `commands/exec.rs`**
   - In the `ExecTarget::PowerShell` arm of `run_exec`: mirror the Python branch — `let code = cmd.join("\n")`, `mkdir exec-results`, build `{token}.json` path, call new `powershell_frame(&code, &path)`, return `WaitMode::SideChannel`.
   - In the `cmd_exec` timeout drain `match`: change `PowerShell` from `drain_until_sentinel` to `drain_until_side_channel`.

4. **Tests — unit (`src/exec_frame.rs`)**
   - `powershell_side_channel_frame_encodes_code_and_path`: assert both inputs are base64-encoded into the frame, neither appears raw.
   - `powershell_side_channel_frame_handles_special_chars`: payload with quotes, backticks, `$variables`, multiline.
   - `powershell_side_channel_frame_handles_windows_paths`: result path like `C:\Users\rick\exec-results\abc.json` round-trips.

5. **Tests — integration (`tests/cli_exec.rs`, Windows-gated)**
   - `exec_powershell_clean_stdout`: simple `echo hello-world` → stdout is exactly `hello-world\n`, no prompt, no framing.
   - `exec_powershell_arbitrary_expression`: `$x = 1; $x + 1` → stdout `2`, exit 0.
   - `exec_powershell_pipeline`: `1..3 | ForEach-Object { $_ * 10 }` → stdout `10\n20\n30`, exit 0.
   - `exec_powershell_state_persists_across_calls`: assignment in one exec readable in next.
   - `exec_powershell_stderr_separated`: `Write-Error 'oops'` → stderr `oops`, stdout empty, exit non-zero.
   - `exec_powershell_cwd_after`: `Set-Location C:\Users` → next exec reports `cwd_after = C:\Users`.

6. **Update the framing plan** (`powershell-exec-framing`, since archived to `docs/plans/completed/2026-05-10-powershell-exec-framing.md`) — mark the framing half done (commit `3f6ff2b`) and link to this plan.

7. **Verify on Win 11 ARM64 VM** — same dev loop as the breakaway fix: cross-compile, scp `tender.exe` to `~/.local/bin/`, restart `ps` session, run integration tests.

## Acceptance Criteria

- All unit + integration tests pass.
- Repro of all three plan failure modes (`echo`, `$x = 1; $x + 1`, pipeline) produces clean stdout in the result envelope (no `PS C:\…>` prefix, no echoed framing line).
- Stderr appears in the `stderr` field, not mixed into stdout.
- `cwd_after` reflects post-exec `Get-Location`.
- Session log (`output.log`) may still contain prompt/transcript noise — that's a cosmetic separate concern and explicitly out of scope.
- The 7 `exec_duckdb_*` tests still pass (no DuckDB regression).
- All existing windows tests still pass.

## Non-Goals

- Cleaning up PowerShell's session log noise (prompt, echoed input). Out of scope; would need either PSReadLine removal at session start or a dedicated result host (pwsh `-NoLogo -NoProfile -NonInteractive -Command -` with framing).
- Changing how `tender exec ps` parses argv. The `cmd: Vec<String>` → `code: String` join (`cmd.join("\n")`) matches the Python pattern. Multi-element argv becomes multi-statement.
- Switching from Windows PowerShell 5.1 to PowerShell Core 7+ (`pwsh.exe`). The frame works on either; choosing one is a separate operational decision.
- Streaming stdout/stderr back to the caller as the script runs (this is request/response, not pub/sub).

## Open Questions

1. **Object output rendering.** PowerShell cmdlets emit objects; `| Out-String` renders them via the default formatter. For most cmdlets this matches what an interactive user would see. For richly-typed objects (e.g. `Get-Process`), the formatted output may be table-style with header rows. Acceptable? Or should we offer a `-Json` switch on `tender exec ps` for structured output?
   - **Default:** keep `Out-String` rendering; document as the same UX as an interactive prompt.
2. **Result file cleanup.** Python doesn't currently delete `{token}.json` after reading. Same convention applies here (kept for post-mortem). Confirm.
3. **Maximum payload size.** No explicit limit today. PowerShell + base64 + stdin pipe should handle several MB without issue. Guard with a sanity ceiling? Probably not yet.

## Estimated Scope

- ~80 lines new in `src/exec_frame.rs` (frame + 3 unit tests)
- ~15 lines changed in `src/commands/exec.rs` (two match arms)
- ~120 lines new in `tests/cli_exec.rs` (6 integration tests, gated on `#[cfg(windows)]`)
- ~20 lines changed in the parent plan doc

Total: roughly one focused commit, ~4–6 hours including the VM verification loop.

## Implementation Notes (2026-05-10)

Final implementation matches the plan shape: a single-line inline frame pushed to PS stdin, with a side-channel JSON result file. **One** behavioral fix beyond the original design is required: the inline frame must be terminated with two newlines (`\n\n`), not one.

### The blank-line requirement

PowerShell's interactive REPL — both Windows PowerShell 5.1 and PS Core 7+ — buffers complex multi-statement input (try/catch, if/elseif/else chains, hashtables, multi-line pipelines) on a sustained stdin pipe and waits for a **blank line** to flush the parser before executing, even when the syntax is already complete. With a single trailing `\n` the line is echoed to the session log but **never executed** — silently. With `\n\n` the same content runs immediately.

This is documented upstream as [PowerShell/PowerShell#3223](https://github.com/PowerShell/PowerShell/issues/3223): *"the end of the multi-line command is never detected, causing it to be quietly ignored… Inserting an empty line after the multi-line command fixes the problem."* The behavior is the same on Windows PS 5.1 and pwsh 7.6.

The frame in [`exec_frame::powershell_frame`] therefore ends with `"…[Move]…)\n\n"`. That single change is the entire deviation from the plan.

### A red herring along the way

Initial diagnosis (since corrected) blamed line length: a 962-byte inline frame ending in `\n` hung, while a 912-byte simple line worked. Bisecting on length led to a file-based pivot (frame on disk + short `& '<path>'` invocation). After verifying that **the same long inline frame runs cleanly on both Windows PS and pwsh when terminated with `\n\n`**, the file-based path was reverted in favor of the simpler inline approach the plan called for. The "long line" symptom was actually "complex multi-statement input without trailing blank line."

### Other tightening

- Test-session start args: `powershell -NoProfile` (interactive REPL). `-Command -` would buffer all stdin until EOF — incompatible with multiple execs against one persistent session.
- Exit-code logic uses `if $_errBuf.Length -gt 0 { $_exit = 1 }` to detect ErrorRecord captures, since `$?` is reset to True by the success of the trailing `ForEach-Object` iteration.
- Object rendering: `($_ | Out-String).TrimEnd()` per item (vs the proposed `Out-String -Stream`) avoids extra blank lines when objects render to multi-line strings.
- pwsh.exe is interchangeable with powershell.exe for this use case — same blank-line requirement, same frame, same result envelope. Choice of host is an operational decision, not a tender concern.

### Verified end-to-end on Win 11 ARM64

All six plan acceptance scenarios pass against `tender exec ps -- <code>` on a `powershell -NoProfile` session: clean stdout, arbitrary expressions (`$x = 1; $x + 1` → `"2"`), pipelines (`1..3 | ForEach-Object { $_ * 10 }` → `"10\r\n20\r\n30\r\n"`), state persistence, stderr separation (`Write-Error 'oops'` → `stderr: "oops\r\n", exit 1`), and `cwd_after` tracking (`Set-Location C:\\` → `"C:\\"`).
