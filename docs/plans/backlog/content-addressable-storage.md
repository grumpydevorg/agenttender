---
id: content-addressable-storage
depends_on:
  - event-emit-primitive
links:
  - ../specs/tender-as-block-runtime.md
---

# Content-Addressable Storage for Captured I/O

> **Superseded/narrowed 2026-07-06 by [event-protocol.md](../specs/event-protocol.md).**
> Slice 1 already ships per-session `events/blobs/<sha256>` for spilled
> payloads (`data_ref`), read via `tender events` — there is no `tender
> block output`, no block records, no global blob root, no refcounts, and
> retention stays prune-owned per-session. Remaining scope of this plan is
> narrowed to extending `data_ref`/blob handling (GC inside prune, more
> capture classes). Cross-session dedupe / a global CAS is **gated on a
> demonstrated consumer need**, since it breaks prune-owned retention. The
> layout, ULID/`parent_block_id` schema, and daemon assumptions below
> predate the protocol and lose to it wherever they disagree.

Persist supervised process I/O (stdout, stderr, large event payloads, captured artifacts) in a content-addressable blob store keyed by sha256. Block records reference blobs by hash rather than embedding bytes. Enables deduplication, replay verification, reproducibility, and crash-bundle export.

## Why

Today Tender's session logs are byte streams on disk per session — flat, append-only, no cross-session deduplication, no integrity check, no way to ask "did this command produce the same output as last time?"

Once events are first-class (see [event-emit-primitive](../completed/2026-07-07-event-emit-primitive.md)), the gap widens:

- a single agent session can produce thousands of tool-call events, many with overlapping outputs
- replays of identical commands produce identical bytes, but stored N times
- there's no way to verify "this stored output is unmodified" — bytes can drift
- crash bundles (failing block + parent chain + outputs) are expensive to assemble because there's no addressability

Content-addressable storage solves all four with one move.

## Goal

Storage shape:

```
~/.local/share/tender/
├── namespaces/<name>/
│   └── events.jsonl           append-only event log (source of truth)
└── blobs/
    ├── sha256/
    │   ├── ab/
    │   │   └── abcd...ef      content addressed by its hash
    │   └── ...
    └── tmp/                   in-flight blobs being captured
```

Block records (with their sha256 references) live as events in the JSONL event log and are materialized into an in-memory index on daemon startup. No SQLite database. See [event-log-analytics](../completed/2026-07-09-event-log-analytics-v1.md) for the analytics path over the same JSONL.

Block records carry hashes:

```json
{
  "block_id": "01HXXX...",
  "stdout_sha256": "sha256:abcd1234...",
  "stderr_sha256": "sha256:9876fedc...",
  "stdout_size": 24115,
  "stderr_size": 412,
  ...
}
```

Reading a block's output:

```
tender block output <id> --stream stdout
   → resolves block.stdout_sha256 → reads blobs/sha256/ab/abcd.../ → streams to stdout
```

## Provenance Hashing

The killer feature: **hashable provenance**. Every block gets a stable "what produced this output" hash derived from its inputs:

```
provenance_hash(block) = sha256(
    canonical_json({
        argv:        block.spec.argv,
        env:         block.spec.env_filtered,   // PATH-style vars excluded
        cwd:         block.spec.cwd,
        stdin_sha:   sha256(block.spec.stdin) or null,
        host:        block.host,
        parent_prov: parent_block.provenance_hash or null,
    })
)
```

This gives:

- **Reproducibility verification**: same provenance → same expected output. Re-run, hash output, compare to prior `stdout_sha256`. Flakes show as hash mismatches.
- **Replay caching**: `tender exec --replay-if-cached` could short-circuit when an identical provenance has produced output before (opt-in; off by default).
- **Audit trails**: a block's provenance chain is a Merkle-like tree of "what led to this state" — every parent contributes its hash, so tampering anywhere up the chain breaks the leaf hash.
- **Crash bundle export**: `tender bundle <block_id>` walks the parent chain, exports the failing block + all causally-related ancestors + their input + output blobs, with hash integrity preserved.

This is the Nix derivation / Bazel action / Buck rule pattern, restricted to "single supervised command" granularity.

## Why Content-Addressable

| Property | Without CAS | With CAS |
|----------|-------------|----------|
| Identical outputs across runs | Stored N times | Stored once |
| Integrity verification | None — bytes can drift silently | sha256 check on read |
| Replay verification | "Did the same command produce the same output?" requires byte comparison | hash compare; O(1) decision |
| Crash bundles | Re-bundle bytes each time | Reference by hash; bundle is small + extractable |
| Cross-host content sharing | Re-transmit on each access | Hash sync; transmit blob once |
| Garbage collection | Reference counting on session lifetime | Reference counting on hash; orphan blobs collectable |

## Design

### Layered

```
┌────────────────────────────────────────┐
│ Event log (events.jsonl)               │
│   block records embedded as events:    │
│     stdout_sha256 ──┐                  │
│     stderr_sha256 ──┤ references       │
│     stdin_sha256  ──┤                  │
│     provenance_hash ┘                  │
│                                        │
│   In-memory index built from log       │
│   gives O(1) block lookups             │
└─────────────────┼──────────────────────┘
                  │
                  ▼
┌────────────────────────────────────────┐
│ Blob store (~/.local/share/tender/     │
│             blobs/sha256/<aa>/<bbcc>…) │
│   - write: atomic, hash-on-the-fly     │
│   - read: stream from path             │
│   - prune: reference-count → GC        │
└────────────────────────────────────────┘
```

### Writing during capture

PTY/pipe output stream is teed:

1. To the daemon's in-memory ring (for `tender log --tail` and live `watch`)
2. To a temp blob `blobs/tmp/<random>` being hashed incrementally
3. On capture-complete (EOF or block end), atomically rename to `blobs/sha256/<aa>/<full-hash>`

No double-write penalty; the temp file is the canonical bytes during capture.

### Blob types

- `stdout/<sha256>` — captured stdout
- `stderr/<sha256>` — captured stderr
- `event-payload/<sha256>` — event payloads that exceed the 256 KiB inline cap
- `artifact/<sha256>` — explicitly captured artifacts (`tender capture <path>` → blob ref)

### Provenance hash discipline

Provenance hashing must be canonical and deterministic:

- env vars: only stable subset (no random PATHs, no `SSH_AGENT_PID`-style variability); filter list documented and versioned with the hash format
- argv: byte-exact, no normalization
- cwd: absolute path, canonical
- stdin: hash of the stdin bytes (already captured)
- host: stable host id (not hostname — use a host UUID from Tender config)
- parent provenance: included recursively, building a Merkle DAG

Bump `provenance_format_version` when canonicalization changes. Old blocks' provenance hashes are still valid for their format; new blocks use the new format. Mixed-format comparisons are explicit errors.

### Garbage collection

```
tender prune --blobs       # GC unreferenced blobs
tender prune --age 30d     # also prune blocks older than 30 days
                            # (with their blob references dropped → GC)
```

Reference counting: each blob has a ref-count of lifecycle events referencing it. Hits zero → GC candidate. Two-phase delete (mark + sweep) so concurrent writers don't race.

### Storage backends

- **Default**: filesystem (above layout)
- **Future**: pluggable backend trait so blobs can live in S3-compatible object storage for fleet deployments

## Scope

- Event-log schema additions: `stdout_sha256`, `stderr_sha256`, `stdin_sha256`, `provenance_hash` fields on lifecycle events
- Blob store (filesystem) with sha256 paths
- Atomic write + hash-on-capture
- `provenance_hash` computed at block finalization
- `tender block output <id>` reads via hash resolution
- `tender prune --blobs` for GC
- Backward compat: existing session logs continue to work; opt-in CAS via config flag during rollout

## Non-goals

- **No object-storage backend in v1** — filesystem only. S3-compatible deferred.
- **No content-encoding/compression in blobs** — bytes are bytes; consumers compress externally if they want.
- **No streaming partial-blob reads from remote hosts** — that's a later plan.
- **No cross-machine blob deduplication** — each host has its own store. Sync is a separate concern.
- **No replay automation** — `--replay-if-cached` is mentioned as a possible future, not in this plan.

## Acceptance criteria

- block records carry `stdout_sha256`, `stderr_sha256`, `stdin_sha256`, `provenance_hash`
- blobs stored under `~/.local/share/tender/blobs/sha256/<aa>/<full>`
- identical outputs from two different sessions share one blob
- integrity check on read; corrupted blob → clear error
- `tender prune --blobs` removes orphans, leaves referenced blobs intact
- `provenance_hash` is stable: same inputs produce same hash across runs
- `provenance_format_version` field present; bumps are explicit
- Documented in README + a dedicated `docs/architecture/07-content-addressable-storage.md`

## Crash bundle export (small follow-up)

Once CAS exists:

```
tender bundle <block_id> [--with-causal-ancestors] [--output bundle.tgz]
```

walks the parent chain, exports:

- the failing block record
- all its ancestors' records
- all referenced blobs (stdin, stdout, stderr, payloads)
- a manifest with provenance hashes

A second tool can verify the bundle (hash-check every blob) and replay any block in isolation. This is the primitive for reproducible crash reports.

## Open questions

1. **Hash algorithm**: sha256 is the obvious default; blake3 is faster but less ubiquitous. Recommend sha256 with a `hash_algorithm` field so future migration is possible.
2. **Inline-vs-blob threshold**: outputs under N bytes are inlined into the lifecycle event's payload, larger go to blobs (event carries the `sha256` reference). Recommend N = 4 KiB to keep small outputs ergonomic and within POSIX's PIPE_BUF atomic-append guarantee. The 256 KiB event-size cap is the upper bound; the 4 KiB threshold is the inline/blob spill point.
3. **Provenance hash inputs — environment subset**: which env vars are part of provenance? The user can configure this per-namespace? Or a global allowlist? Defer until first concrete use case.
4. **Blob store on encrypted volumes**: rely on OS / FDE rather than encrypting in-process. Document the trust boundary.
5. **What about Windows alternate streams / extended attributes**: skipped — we capture POSIX-style stdout/stderr only.

## Depends On

- `event-emit-primitive` — large event payloads need blob storage; defining CAS together avoids retrofitting later.
