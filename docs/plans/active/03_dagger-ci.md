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
unless the open decision below adds a persistent cache. The local warm numbers
are a proxy only: different architecture (arm64 vs amd64), CPU allocation and
engine lifecycle. The recommendation must say which criteria were measured
where.

A hosted **warm** comparison needs a persistent engine. No existing
infrastructure fits without a decision (see "Open decision" below), so none is
set up by this pilot.

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

- **Hosted CI**, cold only (hosted runners have no warm state). Dagger jobs
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

## Open decision (owner)

A hosted warm comparison needs a persistent engine. Options, none set up:

| Option | Persists | Security for a public repo | Cost |
|---|---|---|---|
| Hosted, ephemeral (this pilot) | nothing | isolated VM per job | free (public repo) |
| Self-hosted runner on an existing host | engine, volumes | GitHub advises against it: fork PRs run code on the host; the existing runner groups (Hetzner cax41 ARM64, dreyfus) refuse public repos and dreyfus serves production | no new spend; ARM64 hosts are not the baseline's architecture |
| Remote engine on an existing host, reached from hosted runners over the tailnet | engine, volumes | privileged engine; PR code can poison the shared cache; fork PRs get no secrets and fall back to cold | no new spend; a Tailscale CI secret |
| Dagger Cloud engines | persistent cache (per docs) | traces public by default | Team $50/month; compute pricing unpublished |
| `actions/cache` of the engine's state volume (plus `Swatinem/rust-cache` for the baseline) | engine state, as a cache entry | caches are branch-scoped, so a PR cannot write main's cache | free; restore and save time counts as Dagger's overhead; 10 GB per-repo cache limit |

A fair hosted warm comparison would also give the native baseline a cargo
cache, since `ci.yml` has none today. The last option uses only
infrastructure the repository already uses; it is the one to try if a hosted
warm number is wanted.

## Deliverables

1. #77 — baseline Windows clippy (done, stacked on #75).
2. The pilot PR: this card, the Dagger module, and a `dagger` workflow
   alongside `ci.yml`. Not merged until the recommendation is accepted.
3. Results and a measured adoption recommendation, recorded in this card.
