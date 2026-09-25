---
id: sidecar-lifecycle-guard
depends_on: [sidecar-survives-client-loss]
links:
  - ../completed/2026-09-25-sidecar-survives-client-loss.md
  - ../../architecture/03-run-lifecycle.md
  - ../specs/event-protocol.md
---

# Sidecar lifecycle guard — no spawned child is ever left unsupervised

Follows [#74](https://github.com/grumpydevorg/agenttender/pull/74) (issue #71),
which fixed the one measured case: a lost `start` client ended the sidecar at its
readiness write. That fix is kept verbatim. This card closes the rest of the
class, and lands before the `tendr` rename (#73).

## The defect class (verified on #74's head, 4198609)

After `spawn_child` returns, `run_inner` still had these exits that leave a
live child with no supervisor:

| Step | Exit | Realistic trigger |
|---|---|---|
| stdin transport | `setup_stdin_forwarding(..)?` | `mkfifo` / `CreateNamedPipe` fails |
| publish Running | `write_meta_atomic(Running)?` | disk full, fsync error |
| readiness rewrite | `?` on the meta rewrite inside #74's own fix | EPIPE plus a disk error |
| output capture | `supervise(..)?` on `output.log` open | ENOSPC, EPERM, EMFILE |
| observe exit | `child_wait(child)?` | ECHILD, Windows wait failure |
| panics | `unwrap`/`expect` on the attach and capture paths | dead today, but unguarded |

Nothing in Tender killed such a child afterwards. `reconcile_sidecar_gone`
only relabels `SidecarLost`; `kill` returns on a terminal session;
`start --replace` only deletes a terminal session's directory; `prune` labels.
The crash tests killed the orphan by hand. Windows already kills the tree when
the sidecar dies (the Job Object is kill-on-close) but records nothing.

## Invariant

After `spawn_child` returns, at every instant either (a) the sidecar is alive
and owns the child, or (b) meta is terminal and the child is dead, or (c) the
sidecar died by a true crash (SIGKILL, OOM, abort) and the next reconciliation
reaches (b). There is no fourth state.

## Decisions (Rick, 2026-09-25)

1. `SidecarFailed` gets a new `wait`/`run`/`start` exit code **5**. Code 3 keeps
   meaning "sidecar lost: the child may still be running".
2. Reconciliation in `status`/`wait`/`run` **kills verified-alive orphans**,
   verifying process identity first (PID reuse), and records the outcome.
3. A stdin transport failure **ends the run** with `SidecarFailed`.
4. An `output.log` open failure **degrades**: warn and drain the child's output
   so it cannot block. The existing mid-run capture hang is a separate card
   ([capture-stop-drain](../backlog/capture-stop-drain.md)).
5. `--on-exit` hooks **run** for `SidecarFailed`, with
   `TENDER_EXIT_REASON=SidecarFailed`.
6. The PTY attach socket is **bound before `Running`** is published; a bind
   failure recovers with a visible warning.
7. New event kind **`run.sidecar_failed`**.
8. **`Spawned → Running` typestate** on the in-process `SupervisedRun` guard.
   Persisted state stays runtime-validated: typestate does not survive
   serialization.

## Design

**The guard.** From the moment the child is spawned, a `#[must_use]`
`SupervisedRun<P>` owns it, with its kill handle, `Meta`, the lifecycle event
writer, the session lock and the not-yet-delivered readiness channel.
`SupervisedRun<Spawned>::publish_running` is the only way to `Running`, so the
resources `Running` promises (stdin transport, attach socket) are set up first.
`SupervisedRun<Running>::finish(reason)` is the only normal exit. Dropping the
guard any other way (an error `?`, an early return, a panic unwind) kills the
child through the platform kill path (process group on Unix, Job Object on
Windows) and records `Exited { reason: SidecarFailed { step } }`.

**Readiness is a value, not a `Result`.** Delivery returns a `#[must_use]`
outcome (`Delivered`, `ClientGone`, `AlreadySent`), so it cannot be `?`'d and
the #71 class cannot come back.

**Per step: recover or end.**

| Step | Action |
|---|---|
| readiness write, readiness meta rewrite, `child_pid` breadcrumb | recover, warning |
| `output.log` open | recover: drain output, warning |
| PTY attach bind | recover, warning |
| stdin transport, publishing `Running`, `child_wait` | kill, record `SidecarFailed { step }` |
| terminal meta write | child already exited: salvage, still run hooks |

**Where the failure shows.** Meta: `"status":"Exited","reason":"SidecarFailed",
"step":"…"`, a `sidecar failed at <step>: <error>` warning, provenance
`direct` with `supervision_failed`. Events: durable `run.sidecar_failed`.
`wait`/`run`/`start`: exit 5. Hooks: `TENDER_EXIT_REASON=SidecarFailed`. If
the start client is still waiting, it gets the terminal snapshot.

**If recording the failure fails,** the child is still killed first. The
diagnostic goes to the independent channels still available: the terminal
event salvaged to `~/.tender/lost+found/events.jsonl`, and an `ERROR:` line to a
waiting `start` client. The sidecar never reports a record it did not make.

**True crashes.** `Drop` cannot run on SIGKILL, OOM or abort. When
reconciliation infers `SidecarLost`, it resolves the child identity (meta's
`Running` child, or the `child_pid` breadcrumb when meta is `Starting`), probes
it and kills it only when the identity is verified alive, recording
`orphan_killed` evidence and a warning. An unverifiable process is reported,
not killed.

## Tests

- Debug-only fault hooks beside `TENDER_TEST_ABORT` and `TENDER_TEST_READY_GATE`:
  `TENDER_TEST_FAIL=<step>` injects an error at that step,
  `TENDER_TEST_PANIC=<step>` panics there.
- `tests/sidecar_failure.rs`: every post-spawn step, asserting the recovery or
  termination, the final meta, the event, the `wait` exit code, the hook and
  its `TENDER_EXIT_REASON`, and that the child (and a grandchild) is gone.
  Panic and abrupt-sidecar-death rows included.
- PTY rows: the group kill reaches a PTY session leader and its children.
- Windows: a write to a readiness pipe whose reader is closed fails (the
  premise of #74 there); true-crash rows are Unix-only because Windows already
  kills the tree on sidecar death.

## Order

One PR, three commits: (1) model and vocabulary; (2) the guard, readiness
outcome type and fault-injection tests; (3) reconciler orphan cleanup and docs.
