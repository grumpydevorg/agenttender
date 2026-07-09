---
id: portable-run-launcher
depends_on: []
links:
  - 2026-04-01-run-shebang.md
---

# Portable Run Launcher — Cross-Platform Script Execution

> **Status: Complete.** Extension-based launcher resolution implemented and tested. Full suite green on macOS (22 tests) and Windows (19 tests, 3 Unix-only skipped). Commits `84667c6`, `44bc08f`.

Make `tender run` work reliably on all platforms by replacing the
implicit Bash fallback with explicit interpreter resolution.

Supersedes the platform-specific parts of the completed run-shebang
plan. The shebang parser, `--shell` flag, and core run semantics from
that plan are preserved. This plan adds the extension-to-launcher
mapping and removes the hidden Bash fallback that breaks on Windows.

## Why

Today `tender run script.sh` falls back to `bash` when the script is
not executable. On Windows, bash is not guaranteed — it only works if
Git-for-Windows or WSL happens to be installed. The current test suite
papers over this by injecting Git Bash into PATH, so test greens are
stronger than the actual product contract.

Exec targets (DuckDB, Python, PowerShell) are already genuinely
cross-platform. The `run` command should meet the same bar.

## Contract

- `ExecTarget::None` unchanged — run launches a script to completion,
  not a persistent REPL. No `--exec-target` on run.
- No hidden Bash fallback on Windows.
- No inference from "looks script-like" beyond the explicit supported
  kinds.
- Unknown files hard-fail with clear `--shell` guidance.

## Interpreter Resolution

### Precedence (highest to lowest)

1. `--shell <interpreter>` — explicit override, always wins (already
   implemented as `shell: Option<String>` in run.rs)
2. Executable bit (Unix) / PE header (Windows) — direct execution, no
   interpreter needed, no shebang parsing
3. Extension mapping — deterministic, cross-platform (new)
4. Shebang (`#!...`) — Unix only, for non-executable files without a
   mapped extension (already implemented)
5. Fail with actionable error: "unknown script type, use --shell"

Note: shebang parsing is only used as a fallback for non-executable
files without a recognised extension. If the file is directly
executable, the OS handles shebangs natively — parsing them ourselves
introduces edge cases (env, interpreter args, path differences) that
direct exec avoids.

### Extension-to-Launcher Argv Mapping

Each entry specifies the complete argv prefix, not just a launcher name.

| Extension | Argv prefix (POSIX) | Argv prefix (Windows) | Notes |
|-----------|--------------------|-----------------------|-------|
| `.sh` | `["bash"]` | `["bash"]` | Requires bash on PATH |
| `.ps1` | `["pwsh", "-File"]` | `["pwsh", "-File"]` | Cross-platform PowerShell |
| `.bat`, `.cmd` | N/A | `["cmd", "/c"]` | Windows only; error on POSIX |
| `.py` | `["python3"]` | `["py", "-3"]` | py launcher on Windows |
| `.rb` | `["ruby"]` | `["ruby"]` | |
| `.js` | `["node"]` | `["node"]` | |
| none | Direct exec if executable, else fail | | |

`.ts` is deliberately excluded — no single standard runner exists
(tsx, ts-node, deno, bun). Use `--shell` explicitly.

### Windows Python Choice

Use the `py` launcher (`py -3`) on Windows, `python3` on POSIX. This
follows Python's own documentation and avoids the Microsoft Store alias
trap where `python3` resolves to a failure stub.

### Behaviour for Unknown Files

If the extension is not in the mapping, the file is not executable,
and no shebang is found:

```
error: cannot determine interpreter for 'script.xyz'
  hint: use --shell to specify the interpreter
  example: tender run --shell ruby script.xyz
```

No guessing, no silent fallback.

## Implementation Tasks

1. Add extension-to-launcher argv mapping function (returns `Vec<String>`)
2. Wire precedence chain into `run` command:
   - `--shell` → extension mapping → shebang → fail
   - Remove implicit bash fallback for non-executable files
3. Update error message for unknown types with `--shell` hint
4. Update CLI help text with supported script types
5. Cross-platform tests (see below)

## Testing Strategy

Tests must not depend on Git Bash unless the test is explicitly about
Bash. Each test should either:

- Use a universally available interpreter (Python via the `py`/`python3`
  pattern)
- Skip cleanly with `#[cfg(...)]` or runtime detection when the
  required interpreter is absent
- Test the error path (unknown extension → failure message)

### Test cases

- `.sh` with bash (skip on Windows without bash)
- `.ps1` with pwsh (skip if pwsh not available)
- `.py` with platform-appropriate launcher
- `.bat` on Windows (skip on POSIX)
- Unknown extension → clear error with `--shell` hint
- `--shell` override works
- Shebang-based resolution for extensionless non-executable (Unix only)
- Executable file → direct launch without interpreter

## Not In Scope

- `--exec-target` for run sessions (run stays ExecTarget::None)
- Automatic interpreter installation or version management
- Virtual environment detection
- Remote run (covered by `--host` independently)
- Interactive script execution (that's PTY + attach)
- `.ts` support (no standard runner; use `--shell`)

## Acceptance Criteria

- `tender run script.sh` works on POSIX, fails clearly on Windows
  without bash
- `tender run script.ps1` works on any platform with pwsh
- `tender run script.py` uses `py -3` on Windows, `python3` on POSIX
- `tender run unknown.xyz` fails with actionable `--shell` hint
- `tender run --shell ruby unknown.xyz` works
- No test depends on Git Bash being in PATH unless explicitly testing
  bash
- `cargo test --test cli_run` is green on both macOS and Windows
