---
id: exec-annotation-ergonomics
depends_on: []
links: []
---

# Exec Annotation Ergonomics

**Shipped 2026-07-09 via PR #17.** The routine `tender exec: annotation too
large even after truncation, dropping` stderr warning is gone, and oversized
annotations
now degrade to a compact `exec_truncated` breadcrumb instead of a silent drop.
The write path was isolated into `write_exec_annotation` (`src/commands/exec.rs`)
as a four-rung ladder: full record → field-truncated record → breadcrumb (with
`command`) → minimal breadcrumb (command dropped). The breadcrumb carries
`stdout_len`/`stderr_len` and `stdout_sha256`/`stderr_sha256` (no raw payload),
plus the queryable `hook_exit_code`/`cwd_after`/`timed_out` and the
`block_id`/`event_id` linkage the full A-line uses. The exec JSON envelope and
exit-code behavior are untouched.

Test-covered: four unit tests in `exec.rs` (breadcrumb on double-overflow,
full-record preserved, field-truncated record preserved, and a giant-command
case that exercises the minimal rung) plus one end-to-end test in `cli_exec`
(`exec_oversized_output_is_quiet_and_leaves_breadcrumb`) asserting no stderr
warning and a real breadcrumb in `output.log`.

Deviations from the sketch below: the shipped breadcrumb includes more than the
plan's example JSON — `block_id`/`event_id` for linkage parity with the full
A-line, and `hook_exit_code`/`cwd_after`/`sentinel` for debuggability — and adds
a rung 4 (drop `command`, bound `cwd_after`) so an oversized-command exec can
never overflow the breadcrumb itself, making the "always leaves a record"
guarantee unconditional. The `using-tender` skill §4 was rewritten from a
"filter this noise" workaround to a short breadcrumb note, and the two matching
"known limitations" bullets were retired.

---

Keep `tender exec` usable for large-output commands by removing noisy annotation-overflow warnings and leaving a breadcrumb when the full annotation cannot be recorded.

## Why

`tender exec` writes an `agent.exec` annotation to `output.log` after each exec. That is useful when the payload fits, but the current overflow behavior has two problems:

1. large stdout/stderr can emit a warning on stderr:

```text
tender exec: annotation too large even after truncation, dropping
```

2. if the annotation is dropped entirely, there is no durable record that the exec happened with oversized output

That leaves two bad outcomes:

- operators end up grepping the warning away during normal use
- debugging an oversized exec later becomes harder because the log has no breadcrumb for the dropped annotation

## Goal

Make annotation overflow low-noise and debuggable:

- normal `tender exec` output stays usable in scripts and agents
- oversized annotations leave a small, durable breadcrumb in `output.log`
- the main exec JSON result stays unchanged

## Current State

Today `src/commands/exec.rs` does this:

1. write full annotation
2. if too large, retry with truncated stdout/stderr
3. if still too large, print a stderr warning and drop the annotation entirely

The actual exec result is still returned correctly. This plan is about the annotation side effect, not command execution or exit-code propagation.

## Non-Goals

- changing the `tender exec` JSON envelope
- changing `stdout`, `stderr`, or exit-code behavior for the exec itself
- retrofitting old logs after the fact
- adding full-text search to Tender logs

## Design Direction

### Warning policy

Overflow in the annotation path should not be noisy by default.

Prefer one of these shapes:

- suppress the stderr warning entirely during normal operation
- or gate it behind an explicit verbose/debug mode if Tender grows one

Do not require users to `grep -v` routine overflow noise out of normal `exec` workflows.

### Breadcrumb on drop

If the annotation still cannot be written after truncation, write a tiny fallback annotation instead of dropping silently.

That breadcrumb should include enough information to explain what happened without trying to store the oversized payload:

- event kind
- original stdout/stderr lengths
- whether truncation was attempted
- stable digest of stdout/stderr payloads if cheap to compute

Example shape:

```json
{
  "source": "agent.exec",
  "event": "exec_truncated",
  "run_id": "...",
  "data": {
    "command": ["..."],
    "stdout_len": 1234567,
    "stderr_len": 0,
    "truncated": true,
    "stdout_sha256": "...",
    "stderr_sha256": "..."
  }
}
```

The exact event name can change, but the key property is durable evidence that overflow happened.

## Implementation Tasks

1. Add regression tests for annotation overflow in `tender exec`:
   - large stdout that requires truncation
   - payload large enough that even the truncated annotation cannot fit
   - stderr output remains clean in the default path

2. Refactor the annotation write path in `src/commands/exec.rs`:
   - isolate full write, truncated retry, and fallback breadcrumb write
   - keep the success path identical for normal-sized annotations

3. Change the default warning policy:
   - no routine stderr warning on overflow in normal operation
   - if a warning remains at all, make it opt-in via a future verbose/debug path

4. Write a compact fallback annotation for the final drop case.

5. Document the new behavior if user-visible flags or semantics change.

## Acceptance Criteria

- large exec output does not emit routine overflow noise on stderr by default
- if the full annotation cannot be stored, `output.log` still contains a compact overflow breadcrumb
- the breadcrumb is small enough to fit under the existing annotation size cap
- `tender exec` stdout JSON result is unchanged
- exec exit code behavior is unchanged
