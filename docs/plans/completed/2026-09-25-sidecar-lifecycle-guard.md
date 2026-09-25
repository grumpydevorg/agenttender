---
id: sidecar-lifecycle-guard
depends_on: [sidecar-survives-client-loss]
links:
  - 2026-09-25-sidecar-survives-client-loss.md
  - ../../architecture/03-run-lifecycle.md
  - ../backlog/capture-stop-drain.md
---

# Sidecar lifecycle guard — no spawned child is ever left unsupervised

Follows [#74](https://github.com/grumpydevorg/agenttender/pull/74) (issue #71),
which fixed the one measured case: a lost `start` client ended the sidecar at its
readiness write. This card closed the rest of the class, before the `tendr`
rename (#73). The contract it produced lives in
[03-run-lifecycle.md](../../architecture/03-run-lifecycle.md) (the lifecycle
guard, its recover-or-end table, orphan recovery and limits).

## The defect class (verified on #74's head, 4198609)

After `spawn_child` returned, `run_inner` still had these exits that left a
live child with no supervisor:

| Step | Exit | Realistic trigger |
|---|---|---|
| stdin transport | `setup_stdin_forwarding(..)?` | `mkfifo` / `CreateNamedPipe` fails |
| publish Running | `write_meta_atomic(Running)?` | disk full, fsync error |
| readiness rewrite | `?` on the meta rewrite inside #74's own fix | EPIPE plus a disk error |
| output capture | `supervise(..)?` on `output.log` open | ENOSPC, EPERM, EMFILE |
| observe exit | `child_wait(child)?` | ECHILD, Windows wait failure |
| panics | `unwrap`/`expect` on the attach and capture paths | dead then, but unguarded |

Nothing in Tender killed such a child afterwards: `reconcile_sidecar_gone`
only relabelled `SidecarLost`, `kill` returned on a terminal session,
`start --replace` only deleted a terminal session's directory, `prune`
labelled, and the crash tests killed the orphan by hand. Windows already
killed the tree when the sidecar died (kill-on-close Job Object) but recorded
nothing.

## Decisions (Rick, 2026-09-25)

1. `SidecarFailed` gets exit code **5**; 3 keeps meaning "the child may still
   be running".
2. Reconciliation kills verified-alive orphans, verifying identity first, and
   records the outcome.
3. A stdin transport failure ends the run.
4. An `output.log` open failure degrades: warn and drain. The mid-run capture
   hang is [capture-stop-drain](../backlog/capture-stop-drain.md).
5. `--on-exit` hooks run for `SidecarFailed`
   (`TENDER_EXIT_REASON=SidecarFailed`).
6. The PTY attach socket is bound before `Running`; a bind failure recovers
   with a warning.
7. New event kind `run.sidecar_failed`.
8. `Spawned → Running` typestate on the in-process guard only; persisted state
   stays runtime-validated.

## Resolution

Shipped in the lifecycle PR stacked on #74, in three commits: the model and
vocabulary; the `SupervisedRun` guard with the `ReadyDelivery` readiness value
and the fault-injection matrix (`tests/sidecar_failure.rs`); reconciliation's
orphan kill and these docs. `Platform::child_identity` became infallible
(both backends capture it at spawn), which removed the last pre-guard exit.
Validation, including what was only run in CI, is recorded on the PR.
