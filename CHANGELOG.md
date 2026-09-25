# Changelog

## Unreleased

### Added

- **`SidecarFailed`: the sidecar says when its own supervision failed.** A new
  exit reason (`"reason":"SidecarFailed","step":"<step>"`), a durable
  `run.sidecar_failed` event, exit code **5** from `wait`, `run` and `start`,
  and `TENDER_EXIT_REASON=SidecarFailed` for `--on-exit` hooks, which run for it.
  Exit code 3 (`SidecarLost`) still means the child may be running.

### Fixed

- **No spawned child is left running unsupervised.** Besides the lost `start`
  client fixed below, a failure after spawning could still end the sidecar with
  the child running: the `--stdin` transport, writing `Running`, the readiness
  meta rewrite, opening `output.log`, waiting for the exit, or a panic. Now a
  guard owns the child from spawn: steps that do not threaten the run recover
  with a warning (a failed `output.log` open drains the output so the child
  cannot block; the PTY attach socket is bound before `Running`, so a bind
  failure is visible), and the rest stop the child and record `SidecarFailed`.
- **Reconciliation kills a lost sidecar's orphan.** After a true crash
  (SIGKILL, OOM), `status`, `wait` and `run` found `SidecarLost` but left the
  child running forever. They now kill it, only after verifying its identity,
  and record `orphan_killed`. On the `--after` path the child is found through
  its `child_pid` breadcrumb. The cleanup of a session directory left without
  meta follows the same rule, so it no longer kills a process whose identity it
  cannot verify. Reconciliation now holds the session lock and re-reads meta, so
  it can no longer overwrite a record the sidecar wrote a moment earlier.

- **The sidecar survives losing its `start` client** ([#71](https://github.com/grumpydevorg/agenttender/issues/71)).
  If the `tender start` client died between the sidecar spawning the child and
  reading the readiness message (a closed pane, a killed tool call, Ctrl-C),
  the failed readiness write ended the sidecar and left the child running
  unsupervised: `status` later showed `SidecarLost`, with no `run.exited`, no
  further output and no `--on-exit` hooks. A failed readiness write is now a
  session warning (`readiness not delivered: start client gone`), and the run
  carries on to its normal terminal state.

## v0.2.1 — Security: reject option-shaped `--host` destinations

### Security

- **`--host` destination hardening.** The SSH destination is a bare positional
  argument to the local `ssh` binary, so an empty or option-shaped value (e.g.
  `--host '-oProxyCommand=<cmd>'`) could be parsed by the local ssh as an option,
  enabling **local command execution** when an untrusted value reaches `--host`.
  Tender now rejects empty or `-`-prefixed destinations at the CLI boundary
  (exit 2) and re-checks inside `exec_ssh` / `exec_ssh_frame` so no non-CLI
  caller can bypass it. Valid forms (`user@host`, ssh aliases, IPv4, bracketed
  IPv6) are unaffected. The vector was present in `v0.2.0` on both the general
  `--host` path and the `exec` frame path; exploitation requires an untrusted
  value reaching `--host`, so `v0.2.0` is not yanked.

## v0.2.0 — Agent Terminal Integration

The minimum credible release for reactive process supervision consumers like terminal UIs and agent orchestrators.

### New features

- **`--cwd` and `--env` on start** — child processes launch in the requested working directory with environment overrides. Inherited environment is preserved; overrides are additive.

- **`--namespace` on all commands** — sessions are grouped by namespace (`~/.tender/sessions/<namespace>/<session>/`). Default namespace is `"default"` when omitted. Two sessions with the same name can coexist in different namespaces.

- **`--on-exit` callbacks** — repeatable flag on `start`. Callbacks execute after terminal state is durable and the session lock is released. Callback results stored in `~/.tender/callbacks/<run_id>.json`, keyed by run_id (survives `--replace`). Six `TENDER_*` environment variables exported to callbacks.

- **`tender watch`** — multiplexed NDJSON event stream. Emits `run` and `log` events using the canonical event envelope. Flags: `--namespace`, `--events`, `--logs`, `--from-now`. Polling-based (100ms). Incremental log tailing. Status dedup. Broken pipe = clean exit.

### Architecture

- **Two state machines:** Run lifecycle ends at terminal meta.json write. Callbacks are a separate post-exit workflow, running after the session lock is released. `--replace` is no longer blocked by slow callbacks.

- **Canonical event envelope:** Frozen schema with fields `ts`, `namespace`, `session`, `run_id`, `source`, `kind`, `name`, `data`. Phase 2B emits `run` and `log` kinds from `tender.sidecar` source.

- **Platform trait extended:** `spawn_child` now accepts `cwd` and `env`. Windows skeleton compiles with the new signature.

### Tests

218 tests (up from 178 in v0.1.0). New coverage for namespace isolation, on-exit callbacks, watch event stream, boundary validation, env inheritance, and idempotency with cwd/env/namespace.

### Known limitations

- Watch is polling-based (100ms). Native filesystem backends (kqueue, inotify, ReadDirectoryChangesW) are a future optimization seam.
- Windows backend is signature-compatible but stub-only. Integration tests fail at spawn_child. 4 pre-existing session_fs test failures on Windows.
- No annotation events or `tender wrap` yet (planned for next release).
- No `tender prune` yet (planned for next release).
- Callback timeout is not enforced — a hung callback keeps the sidecar process alive.

## v0.1.0 — Core Local Supervision

Initial release. 8 CLI commands, Unix process supervision, crash recovery, idempotent start, log capture, stdin push, timeout enforcement.
