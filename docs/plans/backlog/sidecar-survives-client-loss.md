---
id: sidecar-survives-client-loss
depends_on: []
links:
  - ../../guide.md
  - https://github.com/grumpydevorg/agenttender/issues/71
---

# Sidecar survives client loss — keep supervising when `start`'s client dies

Tracked in [#71](https://github.com/grumpydevorg/agenttender/issues/71).

If the `tendr start` client is killed after the sidecar has spawned the child
but before it reads the readiness message, the sidecar exits and the child keeps
running **unsupervised**. `status` later infers `SidecarLost`
(`lock_released` + `non_terminal_meta`); nothing records the child's exit, its
remaining output or its `--on-exit` hooks.

Any abrupt end of the client triggers it: closing a herdr pane or terminal while
`start` is returning, an agent harness killing a timed-out tool call, Ctrl-C.
The window is small (a whole `start` takes 10–20 ms here) but real.

## Evidence (2026-09-25, macOS, tender 0.2.1 at d703951)

`tender start --namespace herdr-proof-race raceN -- sleep 20` in the background,
`kill -9` of the client after 4–15 ms, 30 runs: **20 `SidecarLost`, 10
`Running`, and all 30 `sleep` children still alive**. First seen as a herdr
pane closed about 100 ms after its `tender start` was typed. The session's
events end at `run.started` with no `run.exited`.

## Mechanism (read from the code, not instrumented)

In `run_inner` (`src/sidecar.rs`), the no-dependency path spawns the child,
writes `Running` and emits `run.started`, then calls
`signal_meta_snapshot(ready, &meta)?`. With the client gone, the readiness pipe
has no reader, the write fails, and `?` returns from `run_inner`, which ends
the sidecar before `supervise`. The other `signal_meta_snapshot` calls
precede spawning, or follow a spawn failure, so no child is left behind.

## Fix direction

Once the child is running, a failed readiness write should be a warning on the
session, not fatal: the client that asked has gone, but the run it started has
not. Acceptance: kill the client inside the window and the session still reaches
`Exited`, with `run.exited` and its hooks. Decide whether the pre-spawn paths
should likewise proceed or abort cleanly, rather than leave it to `?`.
