---
id: dagger-ci
depends_on: []
links:
  - ../specs/windows-parity.md
---

# Dagger CI

Run the Linux checks (fmt, clippy, package, doc, msrv, test) as Dagger
checks: locally, in GitHub Actions, and on Dagger Cloud Checks after push.
The Windows and macOS lanes in `ci.yml` stay as they are.

Owner's request: run the checks locally, run them again to show the cache,
then configure Cloud Checks to run them automatically after a push, and
explain how Dagger and Nix coexist here. Implementation starts after the
sidecar lifecycle guard (#75) merges.

## Decisions

### Dagger version: stable 0.21.9, not 1.0 beta (2026-09-25)

Outcome: pin `engineVersion` in `dagger.json` and the workflow `version:` to
`0.21.9`. Do not adopt `v1.0.0-beta.N`.

Reason: every concrete requirement of this workstream is met on 0.21.9.
Cloud Checks needs a Dagger Cloud team account, the Dagger Cloud GitHub App
on the org, a `dagger.json` at the repo root and at least one `+check`
function (0.21 docs, `reference/configuration/cloud.mdx`); it does not need
`dagger.toml` or 1.0 workspaces. Checks, generators, toolchains and lockfiles
are all in 0.21. The 1.0 beta has no GitHub release, and its main change
splits `dagger.json` into `dagger.toml` and `dagger-module.toml`; 1.0 still
loads `dagger.json`, so this choice does not block a later migration.

Limits: Dagger runs Linux containers only (0.21 FAQ), so it can take over
only the Linux jobs (`lint (ubuntu)`, `doc (ubuntu)`, `msrv`,
`test ubuntu-latest`). `test macos-latest`, `test windows-latest` and
`test windows-11-arm` remain native GitHub Actions jobs and remain required
status checks ([windows-parity](../specs/windows-parity.md) makes
the native Windows gate a shipped contract). Nix stays the hermetic
build/package gate (`nix flake check`); Dagger does not run Nix and Nix does
not run Dagger. The Dagger container reads `rust-toolchain.toml`, so the
toolchain is still declared once.

Evidence: GitHub release `v0.21.9` (2026-08-26, latest, not a prerelease);
`https://dl.dagger.io/dagger/latest_version` returned `0.21.9`;
`v1.0.0-beta.9` exists only as a tag. Docs read 2026-09-25 from
`dagger/dagger` `docs/versioned_docs/version-0.21/` and
`docs/current_docs/config/migrate-dagger-json.mdx`.

Revisit when a 1.0.0 GitHub release exists, Cloud Checks starts requiring
`dagger.toml`, or a needed feature is 1.0-only.
