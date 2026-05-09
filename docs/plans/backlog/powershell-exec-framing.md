---
id: powershell-exec-framing
depends_on: []
links:
  - powershell-exec-side-channel.md
---

# PowerShell Exec Framing

Fix the PowerShell `exec` target so it can execute arbitrary PowerShell expressions and return clean stdout/stderr without prompt transcript noise.

## Status

The plan splits cleanly into two halves:

- **Framing half — DONE** in commit `3f6ff2b` (`fix(exec): PowerShell frame supports arbitrary expressions`). Argv is now joined and inlined as raw PowerShell code rather than wrapped with `& 'cmd' 'arg'`. Variable assignment, pipelines, and multi-statement payloads work; state persists across exec calls.
- **Transcript-noise half — DEFERRED** to `powershell-exec-side-channel.md`. The captured `stdout` field still contains prompt + echoed framing line because PowerShell's pipe-stdin host emits both. Suppressing them via session-setup proved fragile; the side-channel result-file approach (mirroring `PythonRepl`) is the robust path.

## Why

The current PowerShell frame in `exec_frame::powershell_frame` wraps the argv with:

```powershell
& '...'
```

That only works cleanly for simple command invocation. It breaks for real PowerShell expressions such as variable assignment, pipelines, and multi-statement snippets because `&` expects an invocable command, not an arbitrary expression.

The current capture path also leaks prompt transcript and line-editor escape noise into stdout. Even successful commands can produce output shaped like:

- prompt text (`PS ...>`)
- echoed framing command
- terminal control sequences
- actual result line

That makes the PowerShell lane materially worse than:

- POSIX shell exec
- Python REPL exec
- DuckDB exec

## Goal

Make PowerShell `exec` agent-usable:

- arbitrary PowerShell expressions run correctly
- stdout is clean application output, not prompt transcript
- stderr remains distinct from stdout
- exit code and final cwd are still reported through the Tender result envelope

## Current Failure Modes

1. **Expression failure**
   - `$items = @(1..10) | ForEach-Object { $_ * 2 }`
   - current framing treats the whole expression as a command name

2. **Transcript noise**
   - prompt line
   - echoed framing line
   - ANSI / line-editor escapes
   - actual command output mixed into the same captured stream

## Design Direction

Treat PowerShell as its own execution protocol, not as a shell-quoted argv list.

### Framing

Replace `& 'arg0' 'arg1' ...` style invocation with a PowerShell-native frame that can run script text:

- use a script block or equivalent expression wrapper
- execute the requested payload as PowerShell code, not as an argv-only command call
- preserve cwd-after and exit-code reporting

### Output capture

Do not rely on prompt transcript scraping for correctness.

The frame should produce a clean result channel by:

- suppressing prompt / PSReadLine noise where possible
- separating stdout from stderr intentionally
- keeping Tender sentinel/result reporting out of user-visible stdout payload

If clean sentinel-in-stdout framing proves too fragile, prefer a side-channel result file similar to `PythonRepl`.

## Non-Goals

- implementing a full PowerShell AST parser
- adding PTY support for generic PowerShell exec
- normalizing all PowerShell object output into structured JSON by default
- changing the user-facing `exec` result envelope for other exec targets

## Implementation Tasks

1. Reproduce the current failures in tests:
   - variable assignment + later reuse
   - pipeline expression
   - simple cmdlet output without prompt pollution

2. Redesign `exec_frame::powershell_frame` so it runs script text rather than command-only argv.

3. Decide the result transport:
   - keep sentinel mode only if stdout can be made clean and deterministic
   - otherwise switch PowerShell to a side-channel result file protocol

4. Update `cmd_exec` wait logic if PowerShell moves away from sentinel mode.

5. Add regression tests for:
   - clean stdout
   - stderr capture
   - exit code propagation
   - cwd persistence
   - multi-statement / expression execution

## Acceptance Criteria

- `tender exec ps -- '$x = 1; $x + 1'` succeeds and returns `2` in stdout
- prompt text and terminal control escapes do not appear in captured stdout
- stderr is preserved as stderr
- exit code and `cwd_after` remain accurate
- PowerShell behavior is documented accurately in the README
