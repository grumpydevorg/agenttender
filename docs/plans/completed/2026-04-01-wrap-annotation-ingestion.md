---
id: wrap-annotation-ingestion
depends_on: []
links:
  - 2026-04-01-exec.md
---

# Wrap — Transparent Annotation Ingestion

> **Status: Complete.** Core `wrap` command is implemented and tested (11 integration tests). The `exec` concept (sentinel-framed command execution over persistent shells) has been split into its own plan (since completed at `completed/2026-04-01-exec.md`).

Tap agent hook events without modifying agents or hook scripts.

`wrap` is the public annotation primitive. There is no public `emit`.

## CLI

```bash
tender wrap --session <s> --namespace <ns> --source <src> -- <command> [args...]
```

## What Was Implemented

1. Read all of stdin into memory, then forward to the wrapped command's stdin
2. Capture a copy of stdin for the annotation payload
3. Let the wrapped command run to completion
4. Capture stdout, stderr, and exit code
5. Write annotation event to session output.log
6. Replay stdout/stderr to caller unchanged, exit with child's exit code

Note: stdin is buffered, not streamed. This is fine for hook payloads (small JSON blobs) and simplifies the implementation. A future streaming mode could be added if needed for large inputs.

## Annotation Payload (actual format)

Annotations are written as tagged lines in `output.log` with this envelope:

```json
{
  "source": "agent.hooks",
  "event": "pre-tool-use",
  "run_id": "019...",
  "data": {
    "hook_stdin": { ... },
    "hook_stdout": { ... },
    "hook_stderr": "...",
    "hook_exit_code": 0,
    "command": ["cmux", "claude-hook", "pre-tool-use"],
    "truncated": false
  }
}
```

- `hook_stdin`/`hook_stdout`: parsed as JSON if valid, raw string otherwise
- `hook_stderr`: captured as string
- `truncated`: true if any field exceeds 3KB (`MAX_FIELD_BYTES = 3000`), total annotation line capped at 4KB (`MAX_ANNOTATION_LINE = 4096`)

## Why wrap, not emit

- `wrap` preserves provenance -- Tender observed the actual execution
- `emit` would only carry "some process asserted this happened"
- `wrap` constrains annotation production to observed command executions
- `wrap` matches the real integration path (hook commands already exist)

## exec (split out)

The `exec` concept (sentinel-framed command execution over persistent shells) was originally part of this plan but has been split into its own plan (since completed at `completed/2026-04-01-exec.md`). It needs significant design work (sentinel framing protocol, binary output handling, timeout semantics) that is independent of the completed `wrap` command.
