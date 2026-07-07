---
id: remote-exec-host-parity
depends_on: []
links:
  - ../../architecture/06-transport-boundaries.md
  - ../backlog/exec-annotation-ergonomics.md
---

# `--host` Parity for `exec` — Frame-from-Stdin Transport

Close the worst AX trap in the CLI: `--host` is a global flag, but `exec`
(and `run`/`wrap`/`prune`) silently don't support it. Fix it in two slices —
reject the doomed call loudly first, then make remote `exec` actually work by
shipping the payload over the SSH stdin channel so it never traverses a shell.

## Why

The `--host` split is the most likely first-failure for any agent (or human)
using Tender remotely. The using-tender skill documents four separate
workarounds that all trace back to this one gap:

1. §1 — `--host` appears in global help on local-only verbs, inviting a wasted call.
2. §4 — annotation-overflow stderr noise on large exec payloads (argv-size driven).
3. §5 — nested-quote mangling when `ssh` wraps remote `exec` (three quoting layers).
4. §8 — Windows SSH default-shell quoting differences add a fourth layer.

The litmus test for this plan landing well: **the using-tender skill gets
shorter.** §1, §4, §5, and half of §8 should delete.

Only `exec` genuinely hurts. `run` is a convenience over `start` (which
already works remotely); `wrap` and `prune` are inherently local. They stay
local-only — but they must say so loudly.

## Goal

```bash
# Today (fails silently / confusingly):
tender --host nerevar exec ddb -- "SELECT count(*) FROM t;"

# Slice 1 — fails loudly with the working alternative:
#   error: 'exec' does not support --host yet
#   try:  ssh nerevar 'tender exec ddb -- "SELECT count(*) FROM t;"'
#   (exit 2)

# Slice 2 — just works, with zero shell-quoting traversal:
tender --host nerevar exec ddb -- "SELECT count(*) FROM t;"
```

## Slice 1 — Strict Validation (immediate)

Reject `--host` on `exec`, `run`, `wrap`, `prune` with exit 2 **before any
connection or side effect**. The error message must:

- name the verb and state it is local-only
- print the exact `ssh <host> 'tender exec …'` fallback, pre-filled with the
  user's session and payload (best-effort shell-quoted)
- distinguish exit 2 (usage) from exit 1 (runtime) per error-semantics
  conventions

This is an evening of work and removes the trap even before Slice 2 exists.

## Slice 2 — Remote Exec via Frame-from-Stdin

Do **not** implement remote exec as naive `ssh host tender exec <argv>` — that
re-creates the §5 quoting hell inside the tool. Instead, the payload never
touches a shell:

1. Local CLI serializes the full exec request as one JSON frame:
   `{session, payload, exec_target, timeout, flags…}` (versioned schema).
2. Opens `ssh <host> tender exec --frame-from-stdin` — the remote argv
   contains **nothing user-controlled**.
3. Writes the frame over the SSH stdin channel. Remote tender validates the
   frame and executes exactly as local exec does today (same exec lock, same
   session semantics).
4. Result envelope (already JSON) returns on stdout; inner exit code
   propagates through SSH natively; stderr stays human-only.

### `--frame-from-stdin` is independently useful locally

- Multi-line SQL/Python payloads stop fighting argv quoting entirely.
- Payload size no longer rides in argv → the annotation-overflow path in
  `exec-annotation-ergonomics` largely disappears (link), since the frame can
  carry size/hash breadcrumbs natively.
- Windows quoting (§8) reduces to "get one ssh channel open" — the payload
  itself is opaque bytes.

### Non-goals

- `run`/`wrap`/`prune` over SSH (stay local-only, with Slice 1 errors).
- The sidecar-control-protocol refactor (per-session sidecar IPC, verbs as
  protocol messages, `--host` universal by construction — see
  [sidecar-control-protocol.md](../specs/sidecar-control-protocol.md),
  explicitly **not** a host daemon and not scheduled). That is the right
  end-state but it is an architecture decision, not a `--host` bugfix. This
  plan must not block on it, and Slice 2's frame schema should be designed
  so it can become a message of that future protocol unchanged.

## Acceptance

- [ ] `tender --host h exec …` works end-to-end against a real remote session
      (Linux host), including non-zero inner exit codes and `timed_out`.
- [ ] Payload containing single quotes, double quotes, `$vars`, newlines, and
      backslashes survives byte-exact (test fixture, both directions).
- [ ] `--host` on `run`/`wrap`/`prune` exits 2 with the actionable message.
- [ ] Local `exec --frame-from-stdin` accepts the same frame schema.
- [ ] using-tender skill updated: §1, §4, §5 deleted or reduced; §8 halved.
