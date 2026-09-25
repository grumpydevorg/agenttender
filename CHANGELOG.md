# Changelog

## Unreleased — Renamed to `tendr`

### Breaking changes

The project, crate and binary are renamed from Tender / `agenttender` /
`tender` to **`tendr`**. Nothing reads the old names, so this is a clean break:

- **Crate and binary:** `cargo install tendr` installs the `tendr` binary. The
  library is `tendr` (`use tendr::…`). Release archives are
  `tendr-<target>.tar.gz`. The `agenttender` crate stops at 0.2.1.
- **State root:** `~/.tender` → `~/.tendr` (sessions, callbacks, `lost+found`).
  There is no automatic migration and no fallback read of the old path.
- **Environment variables:** every `TENDER_*` variable is now `TENDR_*`
  (`TENDR_SESSION`, `TENDR_NAMESPACE`, `TENDR_RUN_ID`, `TENDR_GENERATION`,
  `TENDR_EXIT_REASON`, `TENDR_SESSION_DIR`, `TENDR_BLOCK_ID`,
  `TENDR_PARENT_EVENT_ID`, and the internal ready-pipe variables).
- **Script directives:** `#tender: key=value` is now `#tendr: key=value`.
- **Events:** internal sources are `tendr.sidecar`, `tendr.exec` and
  `tendr.cli`, and `tendr.` is the reserved source and kind prefix. Event logs
  written before the rename keep their `tender.*` sources.
- **Exec framing:** the in-band sentinels are `__TENDR_EXEC__` / `TENDR_EXEC_`,
  so `tendr exec` cannot drive a session started by the old binary.
- **Agent skill:** `using-tender` is now `using-tendr`; `tendr skill install`
  writes `.claude/skills/using-tendr/SKILL.md`, and the Nix package installs
  `share/agent-skills/using-tendr/SKILL.md`.
- **Nix:** the flake's package and check are `tendr` (`nix build .#tendr`).

### Cutover

Live sidecars from the old binary hold absolute paths into `~/.tender`, so move
the state root only once none are running:

1. Stop every session the old binary supervises (`tender kill <session>`), or
   let them finish.
2. `mv ~/.tender ~/.tendr`
3. Install `tendr`, remove `tender`, and update scripts, shebangs and callbacks
   that use `tender`, `TENDER_*` or `#tender:`.

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
