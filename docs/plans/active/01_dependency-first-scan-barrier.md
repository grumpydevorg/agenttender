---
id: dependency-first-scan-barrier
depends_on: []
links:
  - ../specs/sidecar-control-protocol.md
---

# Dependency First-Scan Barrier — Readiness After the First Dependency Scan

Strengthen the sidecar→CLI readiness handshake so a `--after` run reports ready
**only after its first complete dependency scan** has latched every
already-satisfied dependency and identified the pending ones. This makes
`tender start … --after …` returning a *proof* that the dependency-wait state is
established, which removes the test-side "sleep 500–1000 ms and hope the scan
happened" race — without a new CLI flag, a new public event, or a new
`RunStatus` variant.

## Why — the readiness/scan inversion

Today the readiness signal fires **before** the first scan. In
`src/sidecar.rs:589-596`:

```rust
if has_deps {
    // Signal readiness BEFORE waiting — CLI unblocks, status shows Starting.
    session::write_meta_atomic(&session, &meta)?;
    signal_meta_snapshot(ready, &meta)?;        // step 3: readiness
    match wait_for_dependencies(...) { ... }     // step 4: first scan
}
```

`tender start` blocks on `Current::read_ready_signal` (`src/commands/start.rs:319`)
and unblocks the instant that snapshot arrives — i.e. after `Starting` is written
but **before any dependency has been examined**. So when `start` returns, the
sidecar owns the session but may not have scanned its `--after` bindings yet.

That is the race, and it is real in the code. `wait_for_dependencies` binds each
dependency to the exact `run_id` captured at launch and **rejects a replacement**
(`src/sidecar.rs:305`):

```rust
if dep_meta.run_id() != dep.run_id {
    return DepWaitOutcome::Failed(format!("dependency {} was replaced …"));
}
```

The satisfied-latch (`satisfied: Vec<bool>`, `src/sidecar.rs:240`, set at `:329`,
skipped at `:274`) is monotonic and correctly generation-bound — but only
*within* the wait loop. Because readiness fires before that loop's first
iteration, this sequence is timing-dependent:

```
start job2 --after job1     # job1 already Exited(ok) @ gen1
start returns               # sidecar up, Starting written, NOTHING scanned yet
replace job1                # job1 -> gen2, now Running
```

Depending on scheduling, job2's first scan either latches job1@gen1 (correct) or
reads job1@gen2 and hits the `:305` replaced-branch → spurious
`DependencyFailed`. Tests paper over this with fixed sleeps.

## The barrier contract

Reorder to:

```
acquire session lock
write Starting
first complete dependency scan
  ├─ latch every already-satisfied dependency (bound to its observed run_id)
  └─ identify pending dependencies
publish the post-scan snapshot over the readiness pipe   ← the single funnel
enter the polling loop (only if still Waiting)
```

Then, once `tender start … --after …` returns:

- the sidecar owns the session (unchanged);
- for a **non-terminal** returned snapshot, every dependency has been inspected
  exactly once — already-satisfied ones **irrevocably latched** to the generation
  observed at scan time, pending ones known to be pending. Otherwise, a
  phase-terminal condition was persisted and returned;
- kill requests and `--replace`s issued afterward are deterministically ordered
  *after* the latch.

It does **not** mean all dependencies are satisfied or that the child is running.
It proves only that the initial dependency-wait posture is established.

## State model — make the invalid states unrepresentable

Raw bindings, typed scanned dependencies, and an exhaustive first-scan outcome:

```rust
/// A dependency after the first scan. Only two states can coexist with
/// continued polling; failure is not one of them (see FirstScanOutcome).
enum ScannedDependency {
    Pending(DependencyBinding),   // bound generation not yet terminal
    Latched,                      // observed satisfied — final; binding not retained,
                                  // as a latched dependency is never re-examined
}

/// Constructible ONLY by `first_dependency_scan`. Its existence is the proof
/// that a scan happened.
struct ScannedDependencies(Vec<ScannedDependency>);

enum FirstScanOutcome {
    ReadyToSpawn,                       // every dep already satisfied
    Waiting(ScannedDependencies),       // some pending, none failed
    Terminal {                          // scan produced, or was pre-empted by, a phase-terminal outcome
        reason: DepFailReason,          // Failed | TimedOut | Killed | KilledForced
        warning: String,
    },
}
```

Why this shape (over a flat per-dep `{ Pending, Latched, Unsatisfiable }`):

- **Before the scan** we hold ordinary `Vec<DependencyBinding>` (the existing
  `LaunchSpec::after`, `src/model/spec.rs:50`). Nothing claims to be scanned.
- **`ScannedDependencies` has a private constructor** — only
  `first_dependency_scan` produces one. `poll_dependencies` takes a
  `ScannedDependencies`, so *entering polling with uninspected dependencies is
  not expressible*. That is the invariant carried in the type.
- **Failure is not a persistent per-dependency state.** A failed / timed-out /
  killed dependency aborts the whole wait phase; it is not something we keep
  polling *around*. Modelling it per-dep would make a `ScannedDependencies`
  holding an `Unsatisfiable` element while other deps keep polling
  representable — a state that never occurs. Keeping the per-dep enum to exactly
  `Pending | Latched` (the two states compatible with ongoing polling) and
  routing all failure into `FirstScanOutcome::Terminal` removes it.
- **`Terminal` covers all four `DepWaitOutcome` failures** (`src/sidecar.rs:210`):
  `Failed`, `TimedOut`, `Killed`, `KilledForced` — not only the replaced/failed
  dependency case.

## The readiness funnel — fire exactly once, on every branch

The enum makes the *states* representable; it does not by itself guarantee
readiness fires exactly once. That is a **linear-consumption** property, provided
by one publication site plus taking the ready writer:

```rust
let outcome = first_dependency_scan(&session_root, &namespace, meta.launch_spec(), …)?;
// Takes &session because the Terminal arm persists DependencyFailed to disk.
let action  = apply_first_scan_outcome(&session, &mut meta, &mut lifecycle, outcome)?;

// Exactly one normal post-scan publication point.
signal_meta_snapshot(ready, &meta)?;

match action {
    Action::Spawn            => { /* fall through to spawn */ }
    Action::Wait(scanned)    => poll_dependencies(scanned, …),
    Action::ReturnTerminal   => return Ok(()),
}
```

Resolved dispatch (the child-spawn stays out of this slice):

```
ReadyToSpawn → signal Starting → spawn immediately
Waiting      → signal Starting → poll
Terminal     → persist DependencyFailed → signal it → return
```

`ReadyToSpawn` deliberately still publishes `Starting`: "dependency posture
established" is not "child spawned", and deferring only the all-satisfied case to
`Running` would split publication across two sites again and pull child-spawn
latency / `SpawnFailed` into this change.

`apply_first_scan_outcome` must:

- leave `meta` as `Starting` for `ReadyToSpawn` and `Waiting`;
- for `Terminal`, `transition_dependency_failed(reason)` + `lifecycle.emit(true)`
  + `write_meta_atomic` — i.e. persist `DependencyFailed` **before** returning;
- return only once `meta` represents the outcome about to be signalled.

Because every arm passes through the one `signal_meta_snapshot`, and the ready
writer is a one-shot (`Option<ReadyWriter>`, taken once — `src/sidecar.rs:51-52`),
"readiness fires exactly once, after the scan, on every branch including
terminal" is structural. A terminal first scan can no longer return `Ok` without
signalling and leave `start` reading EOF.

### Invariant established

```
start returned
  ⇒ either every dependency was scanned once and the wait state is published,
     or a phase-terminal condition pre-empted the scan and was persisted first.
```

Genuine infrastructure failure *before* any valid snapshot exists may still close
the pipe and make `start` return an error — that is the correct outcome (the
`run()` wrapper's `ERROR:` path, `src/sidecar.rs:57-59`).

## Observable behavior change (accepted)

`tender start job --after already-failed-dep` will return the current
**`DependencyFailed`** snapshot instead of a stale optimistic **`Starting`** one.
This is deliberate: it matches how the system already reports immediate lifecycle
outcomes (e.g. an immediate `SpawnFailed`) and makes the returned snapshot
truthful. It remains a **successfully processed `start` command** (the sidecar
launched; the dependency failure rides in the returned meta) — not a CLI error.
Confirm during implementation that no test asserts "`start … --after …` always
returns `Starting`".

## Scope / non-goals

- **No new `RunStatus` variant, no meta.json shape change.** `Starting` still
  covers the wait; the barrier is pure handshake *ordering* over the existing
  private readiness pipe. (Matches the sidecar-control-protocol: no new public
  event for a test-observability need.)
- **The 500 ms dependency poll interval stays** (`src/sidecar.rs:339`) — it is a
  legitimate deadline-bounded poll, not a fudge sleep.
- **Only the latch test needs this barrier.** The other ~6 dependency/`--after`
  sleeps guard "sidecar exists / kill request persists", which `start` returning
  already proves today; they are delete-and-stress candidates handled
  independently of this change.

## Test payoff

The "satisfied dependency stays latched" test becomes deterministic and
sleep-free:

```
start job2 --after job1     # returns ⇒ job1@gen1 latched
replace job1                # gen2 — cannot un-latch gen1
assert job2 launches (does not hit the replaced-dependency failure)
```

## Verification

- Regression: the latch test above, plus a `Terminal`-on-first-scan test
  asserting `start` returns a `DependencyFailed` snapshot (success exit, truthful
  meta) rather than hanging or erroring.
- Delete-and-stress each of the ~6 non-latch dependency sleeps separately.
- Full gates + cross-platform CI (the readiness pipe differs Unix/Windows).
