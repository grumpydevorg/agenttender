---
id: dagger-ci
depends_on: []
links:
  - ../specs/windows-parity.md
---

# Dagger CI pilot

Decide, by measurement, whether Dagger is a useful workflow layer for the
Linux checks. The pilot runs Dagger **alongside** `ci.yml`; it replaces no
lane, changes no required check and leaves `release.yml` untouched. It is also
the rehearsal for edge-platform, which would add Nix packaging, Rugix assembly,
hardware acceptance and resumable rollout on top.

Adopt only if all four hold: equivalent coverage, reliable failure reporting,
easier local reproduction, and overhead within the limits below.

## Scope

In Dagger: the Linux jobs of `ci.yml`, one check per step — `fmt`, `clippy`,
`package` (the `lint (ubuntu)` job), `doc`, `msrv`, `test` (ubuntu, with the
pinned DuckDB CLI) and `clippy-windows`.

Stays native: `test macos-latest`, `test windows-latest`, `test windows-11-arm`
(Dagger runs Linux containers only, and [windows-parity](../specs/windows-parity.md)
makes the native Windows gate a shipped contract), `release.yml`, and the
docs-links workflow.

Baseline first: Windows cross-target clippy landed in `ci.yml` as its own
change (#77, stacked on #75), so that coverage gain is credited to the
baseline, not to Dagger. The pilot branch stacks on #77.

## Decisions

### Dagger version: stable 0.21.9, not 1.0 beta (2026-09-25)

Pin `engineVersion` in `dagger.json` and the CLI download to `0.21.9`. Every
requirement here is met on 0.21.9; the 1.0 beta has no GitHub release, and 1.0
still loads `dagger.json`, so this does not block a later migration. Evidence:
GitHub release `v0.21.9` (2026-08-26, latest, not a prerelease);
`v1.0.0-beta.9` exists only as a tag. Note docs.dagger.io now labels 0.21 "no
longer actively maintained". Revisit when 1.0.0 is released or a needed feature
is 1.0-only.

### Module: Go SDK, at the repository root (2026-09-25)

`dagger.json` at the root (Cloud Checks requires it there, should it ever be
adopted) with sources under `.dagger/`. Go is the most complete SDK; its
module-load cost on a cold engine is measured, not assumed — the 0.21 docs
give no SDK comparison.

Explicit inputs: the build toolchain is read from `rust-toolchain.toml` and the
MSRV from `Cargo.toml` `rust-version` (each still declared once); images are
exact tags (`rust:<version>-slim-bookworm`); DuckDB is pinned to the same
version as `ci.yml`. Tests run as an unprivileged user, because several tests
chmod a path to `0o000` and expect `EACCES`, which root ignores.

Caches: one cache volume for the cargo registry, and one target-directory
volume per check and toolchain, so concurrent checks never share a cargo build
lock across containers (unverified to be safe) and a volume name carries every
input that changes its contents.

### What persists on the CI runners (verified 2026-09-25)

Three distinct caches, each with a different lifetime:

| Cache | Keyed by | Survives on a GitHub-hosted runner? |
|---|---|---|
| Dagger operation cache (layers, function results) | inputs: source digest, args, module source | No — engine state dies with the VM |
| Cache volumes (cargo registry, target dirs) | volume name, scoped to the module | No — same engine state |
| Rust build artifacts | live inside the target-dir volumes | No |

Sources (dagger/dagger @ v0.21.9): `dagger-for-github` v8 only installs the
CLI and runs one command, with no caching (`action.yml`); the CLI starts its
own engine on the runner (`ci-integrations/github-actions.mdx`); the runner is
a fresh VM per job (GitHub `secure-use.md`). `_EXPERIMENTAL_DAGGER_CACHE_CONFIG`
is still parsed by the client but nothing in the engine consumes it since the
BuildKit solver was removed in v0.21.0 (inferred from source, not a documented
removal). Any source change misses the operation cache for the step that
consumes it; only the target-dir volume makes that rerun incremental.

Consequence: on hosted runners every Dagger run is cold, and so is every
baseline run (`ci.yml` has no cargo cache). The hosted comparison is therefore
**cold against cold**, and the **warm one-edit limit is not evaluated on CI**
unless the open decision below adds a persistent cache. (Superseded: an
Actions-cache warm experiment was run on existing infrastructure; see
Results.) The local warm numbers
are a proxy only: different architecture (arm64 vs amd64), CPU allocation and
engine lifecycle. The recommendation must say which criteria were measured
where.

A hosted **warm** comparison needs a persistent engine. No existing
infrastructure fits without a decision (see "Open decisions" below), so none is
set up by this pilot. (Superseded in part: the Actions cache was tried as a
stand-in; see Results.)

### Trace export: off, and proven off (2026-09-25)

The CLI uploads traces to Dagger Cloud when `DAGGER_CLOUD_TOKEN` is set or a
`dagger login` credential exists (`engine/telemetry/cloud.go`), and sends
anonymous usage analytics unless `DO_NOT_TRACK=1` (`analytics/analytics.go`);
traces from a public repo's CI are public by default (`cloud.mdx`). Rather than
rely on an unset token or on `HOME` isolation:

- CI sets `DO_NOT_TRACK=1` and `DAGGER_NO_UPDATE_CHECK=1`, passes no token, and
  installs the CLI directly (pinned tarball, checksum-verified), not through
  `dagger-for-github`, whose `cloud-token` input overrides any job-level token.
- The CLI runs in a container with `--network none` and only the Docker
  socket mounted (`.github/dagger-sandboxed.sh`, used by CI and locally alike).
  The Docker daemon starts the engine and does its pulls outside the sandbox;
  the CLI reaches the engine over that socket, and has no route to anything
  else. CI proves the sandbox has no route (a request to `api.dagger.cloud`
  must fail) and fails if a "Full trace at" line appears.
- Limit: this proves the CLI cannot upload. The engine is given no token, so it
  has nothing to upload with; that part is asserted, not sandboxed.

Trace export to Dagger Cloud is a separate decision, tied to Cloud Checks.

### Cloud Checks: separate, unverified (2026-09-25)

Supersedes this card's earlier plan to configure Cloud Checks after push. It
requires a Dagger Cloud **Team** account ($50/month, dagger.io/pricing), the
Dagger Cloud GitHub App on the org, and trace upload. Nothing in this pilot
enables it or claims it works; it is its own integration test, taken only on
the owner's decision.

## Measurement protocol

Acceptance limits, fixed 2026-09-25 **before** any comparison run:

| Criterion | Limit |
|---|---|
| Warm one-edit execution | Dagger adds ≤ max(15 s, 10 % of baseline) |
| Cold execution | Dagger adds ≤ max(60 s, 20 % of baseline) |
| Coverage | identical: same commands; the multiset of per-binary `test result` counts (passed/failed/ignored) equal to the native ubuntu lane on the same commit |
| Failure propagation | a failing check fails its job, the job's status, and `dagger check`'s exit code |
| Required-check enforcement | each Dagger job reports a stable status context that branch protection could require |

Workloads (same commit on both sides):

- **cold**: no Dagger engine state and no cargo target dir;
- **unchanged**: rerun with nothing changed;
- **one-edit**: rerun after changing one line in `src/` (a comment in a
  module every target compiles).

Where each runs:

- **Hosted CI**, cold (hosted runners have no warm state); a warm variant
  through the Actions cache was added later (see Results). Dagger jobs
  mirror the baseline's job split (`lint` = fmt+clippy+package, `doc`, `msrv`,
  `test`, `clippy-windows`), one GitHub-hosted `ubuntu-latest` runner each, so
  the difference per job is Dagger's own cost. Report per-job duration and the
  Linux critical path.
- **Local**, all three workloads, on the same machine and Docker VM. The
  baseline is the same commands in a plain container of the same image with a
  persistent target dir, so the difference is Dagger's, not the OS's; native
  macOS `cargo` is reported as context only.

Phases recorded separately: queue (run start → job start), setup (checkout,
toolchain or CLI install, image pulls, engine start, module load), compilation
(cargo's `Finished … in` lines) and test execution (the test step or check
minus compilation). The baseline installs its toolchain inside its first cargo
step (`rust-toolchain.toml` pins 1.98.0, which `rustup update stable` does not
install); that time is counted as setup, from rustup's own log lines. Record
the runner image and version, vCPU/RAM, and the cache state of each run.

Two workload differences are reported, not treated as noise: the Dagger `lint`
job runs fmt, clippy and package concurrently in three containers with
separate target dirs, where the baseline runs them in sequence in one; and
coverage by counts cannot see a test that returns early when a tool is
missing (the known sites — `duckdb`, enforced by `TENDER_REQUIRE_DUCKDB_TESTS`,
and `shasum`, shipped with `perl` — are checked by hand).

On one-edit, cargo must recompile only the edited crate, not the
dependencies; Dagger materialises the source with its own file timestamps, and
cargo decides freshness by mtime. Record the `Compiling` lines of each run.

At least five runs per workload and side; report median, min, max and every
sample. No run is dropped without a stated reason.

Baseline so far (10 successful `ci.yml` runs, 2026-09-19 → 25, image
`ubuntu-24.04` 20260828.587, 4 vCPU / 16 GB public-repo runners): job medians
lint 39 s, doc 22 s, msrv 28 s, test 120 s. The test step took 111–123 s in
nine runs and 226 s in one. The test job is mostly execution — compilation is
26–33 s of it — so a warm cache can shorten at most that part. The native
ubuntu lane reports 54 `test result` lines, 713 passed, 0 ignored.

## Verification beyond green

- A deliberately failing check (a throwaway draft PR that adds a clippy
  finding and a failing test): the Dagger job, its status and the exit code
  all fail; the other checks still report.
- Cancellation: cancel an in-flight run (`gh run cancel`) and a local
  `dagger check` (SIGINT); record time to stop and whether the engine left
  execs running.
- Required-check reporting: list the status contexts the Dagger jobs publish
  and confirm they are stable across runs. Branch protection is not changed.
- Cache invalidation: locally, an unchanged rerun must hit the cache; then an
  edit that introduces a clippy finding must fail (a stale cache would pass),
  and reverting it must pass again.

## Results (2026-09-25)

All runs are at `0ebcc54` (PR #78) or its one-line edits. Hosted runners:
`ubuntu-24.04` image 20260828.587, 4 vCPU / 16 GB, identical on both sides.
Local: Apple M3 Max, OrbStack Docker VM with 16 CPUs / 16 GB, arm64.

### Coverage: equivalent

- Same commands and flags per check, same toolchains (1.98.0; 1.85.0 for
  MSRV).
- The test check's result lines match the native ubuntu lane exactly: 54
  `test result` lines, 713 passed, 0 ignored, same per-binary multiset.
- The known early-return sites have their tools: DuckDB is required by
  environment, and `shasum` comes with `perl`.

### Failures: reliable

- **Deliberate failure (#79, closed):** a clippy finding and a failing test.
  - `dagger lint`, `dagger clippy-windows` and `dagger test` failed with exit
    code 1; `dagger doc` and `dagger msrv` passed.
  - Inside the failed lint job, `fmt` and `package` still reported.
  - `ci.yml` failed the same jobs.
- **Status contexts:** the workflow publishes five, `dagger lint`,
  `dagger doc`, `dagger msrv`, `dagger test` and `dagger clippy-windows`.
  - Identical on every commit, including the failing one.
  - Published by the same GitHub Actions app (id 15368) as the existing
    required checks.
  - No `paths:` filter, so they always report and branch protection could
    require them. Branch protection is unchanged.
- **Cancellation, hosted:** `gh run cancel` during the checks marked all five
  jobs `cancelled`. The Checks steps stopped 16–21 s after the request, which
  is GitHub's own signal path; the runner VM is discarded anyway.
- **Cancellation, local:** SIGINT to the CLI ended it in under 1 s ("context
  canceled", exit code 2). Sampled with `docker exec … ps`, the engine had 5
  cargo/test processes before the signal and none at every sample from 1 s
  to 30 s after.
- **Cache invalidation, local:**
  - An unchanged `clippy` hit the cache (0.0 s).
  - An injected clippy finding failed it in 2.8 s, through an incremental
    rebuild, not a stale pass.
  - After the revert it passed from the cache again.
  - A check that failed is not cached: the next unchanged run re-executed it.
- **Trace export:** every hosted run passed the no-route proof and printed no
  "Full trace" line. Since `acba47b`, which inspects every dagger step, all five
  jobs log "tracing not configured".

### Hosted, cold (6 paired attempts, fresh runners each)

| Job | native median [range] | Dagger median [range] | added, paired per attempt | limit: max(60 s, 20 %) |
|---|---|---|---|---|
| lint | 41 [40–42] | 91 [84–96] | 42, 47, 48, 51, 54, 55 | 60: **6/6 within** |
| doc | 22.5 [20–24] | 73.5 [70–76] | 48, 50, 50, 51, 52, 55 | 60: **6/6 within** |
| msrv | 28 [25–30] | 84.5 [73–98] | 45, 50, 54, 62, 68, 68 | 60: **3/6 within** |
| test | 127 [125–133] (5 successful; attempt 5 hit a flake) | 175 [168–181] | 38, 38, 46, 50, 54 | 60: **5/5 within** |
| clippy-windows | 43 [32–45] | 97 [89–102] | 46, 46, 50, 56, 59, 70 | 60: **5/6 within** |

Where the time goes (medians):
- **Compilation matches:** test 33.4 s native vs 34.6 s Dagger; msrv 17.5 vs
  16.3.
- **Test execution matches:** 79.4 s vs 79.3 s.
- **All of the added time is setup.** Dagger adds 48–52 s of setup per job
  at the median (38–70 s paired), paid on every hosted job:
  - image pulls 12 s [7–25], of which the engine image is 631 MB;
  - engine start plus the Go module's first compile 22 s (18.5 s is the
    module load);
  - building the check containers inside the engine 10–20 s;
  - the no-route proof 5 s. It belongs to the trace-off decision, so it
    counts; without it msrv would be within the limit in 4 of 6 attempts, and
    no verdict changes.
- Queue time is 2–6 s on both sides.
- Compilation of the lint job is not comparable: Dagger runs fmt, clippy and
  package concurrently in three containers with separate target dirs (the
  longest compile is 23 s), where the native job runs them in sequence
  (26.7 s in total).

The Linux critical path grows from 127 s to 175 s. The PR's wall clock does
not, because it is set by the native Windows lanes (~246 s).

### Hosted, warm, on existing infrastructure (Actions cache)

A throwaway branch (never merged; its caches, 13.5 GB, deleted afterwards) ran
the same five lanes on both sides:
- native with `Swatinem/rust-cache`;
- Dagger with its whole engine state (`/var/lib/dagger`, stopped engine,
  zstd tar) saved and restored through `actions/cache`.

It ran a seed, five one-line edits (warm one-edit), then three reruns (warm
unchanged).

| Lane | one-edit: native / Dagger median | added (limit max(15 s, 10 %)) | unchanged: native / Dagger |
|---|---|---|---|
| lint | 27 / 103 | +76 | 23 / 70 |
| doc | 24 / 78 | +54 | 19 / 57 |
| msrv | 25 / 72 | +47 | 28 / 59 |
| test | 112 / 207 | +95 | 108 / 109 |
| clippy-windows | 26 / 84 | +58 | 28 / 83 |

- The engine state is 1.1–2.2 GB per lane after zstd.
- The restored state worked: engine start fell from 22 s to 7 s (the module
  load was cached), and the checks took 3–6 s (test: 90 s).
- Moving the state cost 38–72 s per one-edit run: restore plus unpack
  13–30 s, and pack plus save 23–42 s, paid on every run.
- Warm was slower than cold on lint, doc and test, and faster on msrv and
  clippy-windows.
- The unchanged reruns hit Dagger's operation cache (each check 0.1 s).
- **The design could save the state only on a cache miss,** which removes
  23–42 s. That would bring msrv to about +13 s; the others would stay at
  +21–36 s. Pulling the engine image (~12 s), starting it (7 s) and restoring
  the state (≥ 13 s) already exceed 15 s. Without restored state, engine
  start and the container build cost 22 s plus 10–20 s. **Result:** with
  existing infrastructure, the warm one-edit limit is out of reach for every
  design.

### Local (5 ABBA rounds, all seven checks concurrently)

The baseline is the same commands in plain `docker run` containers built from
the same recipe, with persistent per-lane target volumes.

| Workload | baseline median [range], all | Dagger median [range], all | limit |
|---|---|---|---|
| cold | 168 [149–172]: 168, 169, 165, 172, 149 | 173 [145–177]: 177, 177, 155, 173, 145 | +5 s ≤ 60: within |
| unchanged | 86 [85–86] | 0.7 (81 once, after a failed run) | — |
| one-edit | 93 [91–94]: 93, 94, 94, 91, 91 | 86 [83–87]: 86, 87, 86, 84, 83 | −7 s ≤ 15: within |

- On one-edit both sides recompiled only `agenttender`, in every run.
  Dagger's handling of file timestamps does not defeat cargo's incremental
  builds.
- Unchanged, Dagger returns cached results and does not re-run the tests.
  The baseline does. That is a behaviour difference, not only a speed-up.
- These numbers are arm64, on a warm persistent engine. They do not predict
  hosted runs.

### Intermittent tests (not Dagger's)

Four existing tests failed intermittently on both sides today:
`exec_oversized_output_is_quiet_and_leaves_breadcrumb`,
`push_to_session_without_stdin_fails`,
`harness_deadline_reports_timeout_with_command_and_deadline`, and
`push_resolves_session_in_namespace` (native Windows ARM64, #77).

The first had already failed 6 times in the last 40 failed `ci.yml` runs.
Locally they failed 6 of 15 baseline runs and 3 of the 11 Dagger runs that
executed tests. The other four Dagger runs were unchanged reruns returned
from cache. Failed samples are kept in the timings above.

## Recommendation

**Do not adopt Dagger for the hosted Linux lanes.** Three of the four criteria
hold:
- **Coverage:** equivalent.
- **Failure reporting:** reliable.
- **Local reproduction:** easier (see below).

The overhead criterion fails:
- **Warm one-edit:** fails by 47–95 s on every lane with existing
  infrastructure. Saving the state only on a cache miss would bring msrv to
  about +13 s; the others would stay at +21–36 s, because pulling the engine
  image, starting it and restoring its state alone exceed 15 s.
- **Cold:** passes on medians (added 47–57 s against 60). It exceeds the limit
  in 4 of 29 paired attempts, each with a slow engine-image pull, so there is
  no headroom.

The overhead is fixed per job (engine, module, containers). A shorter check
cannot hide it. Only a persistent engine removes it, and every persistent
option needs a decision below.

**Local reproduction is a genuine gain.** The in-repo module is one
definition of the Linux checks that runs unchanged locally and in CI:
- It includes the Linux test suite, which native macOS `cargo` cannot run.
- It costs no more than plain Docker (+5 s cold, −7 s one-edit).
- Unchanged checks return instantly.

Limits: on a Mac it runs arm64 Linux, not the runners' amd64, and it needs a
Linux `dagger` binary and a privileged engine container. If that is wanted, keep the module and the
sandbox script. Either drop `dagger.yml` or reduce it to one job on pushes to
`main` that keeps the module from drifting from `ci.yml`. It should not be a
per-PR duplicate of the native lanes.

**For edge-platform:** the fixed 45–60 s per job matters less against
multi-minute Nix and Rugix builds. There, a private repository makes a
self-hosted persistent engine acceptable, and that removes the cost measured
here. That pilot should measure that configuration, not hosted-ephemeral.

## Open decisions (owner)

1. **What to keep:**
   - (a) close #78 and keep only #77;
   - (b) merge the module and sandbox script for local use, with `dagger.yml`
     cut to a drift check on `main`;
   - (c) keep #78 open while a persistent-engine option is tried.
2. **A persistent engine for hosted warm runs**, only if (c). None is set up.

| Option | Persists | Security for a public repo | Cost |
|---|---|---|---|
| Self-hosted runner on an existing host | engine, volumes | GitHub advises against it: fork PRs run code on the host. The existing runner groups (Hetzner cax41 ARM64, dreyfus) refuse public repos, and dreyfus serves production | no new spend; the ARM64 hosts are not the baseline's architecture |
| Remote engine on an existing host, reached from hosted runners over the tailnet | engine, volumes | privileged engine; PR code can poison the shared cache; fork PRs get no secrets and fall back to cold | no new spend; a Tailscale CI secret |
| Dagger Cloud engines | persistent cache (per docs) | traces public by default | Team $50/month; compute pricing unpublished |
| `actions/cache` of the engine state | engine state | branch-scoped caches | **measured: transfer 38–72 s; even without the save step, the fixed engine cost exceeds the 15 s limit** |

3. **Cloud Checks:** still untested. It needs the Team plan and trace upload.
4. **The four intermittent tests** are a product issue, independent of this
   pilot. They deserve their own card.

## Deliverables

1. #77: baseline Windows clippy (stacked on #75).
2. #78 (draft): this card, the Dagger module, the sandboxed `dagger`
   workflow.
3. Results and recommendation: above.
