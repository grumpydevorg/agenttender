# Session Storage

Tender persists one directory per session under `~/.tender/sessions/<namespace>/<session>/`. Some files are durable record, some are transient control breadcrumbs, and some exist only for specific execution lanes.

```mermaid
flowchart TD
    Root["~/.tender/"] --> Sessions["sessions/"]
    Root --> Callbacks["callbacks/<run_id>.json"]

    Sessions --> Namespace["<namespace>/"]
    Namespace --> Session["<session>/"]

    Session --> Meta["meta.json\nDurable run snapshot"]
    Session --> Lock["lock\nSession ownership"]
    Session --> Log["output.log\nJSONL stdout/stderr/annotations"]
    Session --> Events["events/<segment>.jsonl\nDurable lifecycle/provenance history"]
    Session --> EventBlobs["events/blobs/<sha256>\nDurable spilled event payloads"]
    Session --> EventLock["events/append.lock\nTransient append lock"]

    Session --> Launch["launch_spec.json\nTransient pre-sidecar handoff"]
    Session --> Generation["generation\nTransient replace hint"]
    Session --> ChildPid["child_pid\nTransient orphan breadcrumb"]

    Session --> Stdin["stdin.pipe\nConditional: --stdin"]
    Session --> ExecLock["exec.lock\nConditional: exec serialization"]
    Session --> ExecResults["exec-results/<token>.json\nConditional: PythonRepl + PowerShell exec"]

    Session --> KillReq["kill_request / kill_request.tmp\nTransient control file"]
    Session --> KillForced["kill_forced / kill_acted\nTransient kill classification"]

    Session --> AttachSock["a.sock.path\nConditional: PTY breadcrumb"]
    Session --> CaptureErr["capture_errors.log\nBest-effort diagnostics"]
```

This split follows Theme 4: Durable Truth, Derived Views; see [../design-principles.md](../design-principles.md). `meta.json` is the current run snapshot, `output.log` is the child-output record, and `events/*.jsonl` is lifecycle/provenance history. `status`, `list`, `log`, `watch`, `events`, `query`, and any future `graph`/`check` commands project those authorities rather than creating new state.

Durable by design:

- `meta.json`
- `output.log`
- `events/*.jsonl`
- `events/blobs/*`
- `callbacks/<run_id>.json`

Transient / control-plane artifacts:

- `launch_spec.json`
- `generation`
- `child_pid`
- `kill_request*`
- `kill_forced`
- `kill_acted`
- `exec.lock`
- `exec-results/*`
- `stdin.pipe`
- `a.sock.path`
- `events/append.lock`

Session directory rules:

- `meta.json` is written atomically via temp-file + rename.
- `output.log` is append-only JSONL:
  - `O` for stdout / PTY merged output
  - `E` for stderr on pipe sessions
  - `A` for annotations written by `wrap` and `exec`
- `events/*.jsonl` is segmented, append-only lifecycle/provenance history;
  oversized event payloads spill into content-addressed `events/blobs/`.
- PTY attach uses a Unix socket stored in the system temp directory, with `a.sock.path` as the breadcrumb back to the real socket path.

What this diagram omits:

- the run state machine encoded inside `meta.json`
- the specific control semantics of `stdin.pipe`, `kill_request`, and PTY attach
