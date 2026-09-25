# Dagger CI pilot: original samples

Samples behind [the pilot card](../2026-09-25-dagger-ci.md), all from
2026-09-25. Durations are seconds. Hosted rows come from the GitHub Actions
API (job and step timestamps) and job logs, which GitHub keeps under the run
IDs below.

| File | Rows | What |
|---|---|---|
| `hosted-cold.csv` | one per job per attempt | Paired cold runs: `ci.yml` run 36130825651 (`native`) and `dagger.yml` run 36130825712 (`dagger`), attempts 1–6, commit `0ebcc54` |
| `hosted-cold-dagger-steps.csv` | one per step | Step durations of the Dagger jobs in the same runs |
| `hosted-warm.csv` | one per job per run | Warm experiment on a throwaway branch: seed, five one-line edits, three unchanged reruns (run IDs in the file). `native` used `Swatinem/rust-cache`, `dagger` restored and saved its engine state through `actions/cache` |
| `local.csv` | one per benchmark run | Five ABBA rounds on an Apple M3 Max (OrbStack, 16 CPUs, arm64): all seven checks concurrently, as plain `docker run`s (`baseline`) or through `dagger check` (`dagger`) |
| `baseline-history-steps.csv` | one per step | Ten successful `ci.yml` runs from before the pilot (2026-09-19 to 25) |

Column notes:

- `hosted-cold.csv`:
  - `queue_s` is run-attempt start to job start.
  - `setup_s` is everything outside the work steps, plus time inside them
    before cargo's first output (rustup's toolchain auto-install on the
    native side; image pulls, `apt-get` and `rustup` inside the engine on the
    Dagger side).
  - `compile_s` is the sum of cargo's `Finished … in` lines. The Dagger `lint`
    job is the exception: it runs its three cargo invocations concurrently, so
    its value is the longest one.
  - `exec_s` is the rest of the work steps.
  - Native attempt 5 of `test` failed on an intermittent test (#76) and is
    excluded from medians.
- `hosted-warm.csv`: `cache_steps_s` sums the restore, unpack, pack and save
  steps (the `rust-cache` step on the native side); `work_s` is the step that
  ran the checks.
- `local.csv`:
  - `wall_s` includes image builds on the baseline side, and engine start on
    the Dagger side when the run is cold.
  - `crates_compiled` counts cargo's `Compiling` lines.
  - `failed_tests` names the tests that failed, all of them intermittent (#80,
    #76).
- `baseline-history-steps.csv` omits the queue column: the collection
  script's queue figure was wrong for all but the first job of each run.
