# Persistence Architecture

**Status:** Superseded in substance by [event-protocol.md](event-protocol.md)
(2026-07-06) — except the broad **no-transactional-DB** stance, which stands.
Specifically superseded: the per-host daemon and in-memory index (rejected in
a judged bake-off), namespace-level `events.jsonl` (storage is per-session
`events/<seg-uuidv7>.jsonl`), global `monotonic_seq` (→ per-writer `seq`),
ULID identity (→ UUIDv7), the `event_id`/`block_id` schema table (→
event-protocol §1/§10), the PIPE_BUF atomicity claim (retracted — wrong for
regular files), and this doc's headline principle: event-protocol v1 keeps
**meta.json as current-state authority and `events/` as history authority**
— the event log is not the sole source of truth. Do not implement from this
doc.  
**Date:** 2026-05-23  
**Composes with:** [tender-as-block-runtime.md](tender-as-block-runtime.md), [content-addressable-storage](../backlog/content-addressable-storage.md), [event-log-analytics](../active/00_event-log-analytics-v1.md)

How Tender stores and queries every kind of state it accumulates. Three primitives, two file formats, **no transactional database**.

## Principle

> The event log is the source of truth. Everything else is a cache.

If a cache is lost, it can be rebuilt by replaying the log. If the log is intact, no data is lost. This single discipline collapses migration, debugging, disaster recovery, and audit into the same problem.

## The three storage primitives

```
┌───────────────────────────────────────────────────────────────┐
│ ① EVENT LOG     append-only, source of truth                  │
│                                                                │
│   namespaces/<name>/events.jsonl       (hot writes)            │
│   namespaces/<name>/events/*.parquet   (cold, rolled-over)     │
│                                                                │
│   one event per line / row                                     │
│   schema_version on every event                                │
│   monotonic_seq per namespace + ts (wall-clock)                │
└───────────────────────────────────────────────────────────────┘

┌───────────────────────────────────────────────────────────────┐
│ ② IN-MEMORY INDEX     HashMap, rebuilt from log on startup    │
│                                                                │
│   HashMap<BlockId, BlockState>                                 │
│   secondary indexes: by namespace, by parent, by tag           │
│                                                                │
│   optional checkpoint snapshot for fast restart                │
│   never canonical; the log can rebuild it                      │
└───────────────────────────────────────────────────────────────┘

┌───────────────────────────────────────────────────────────────┐
│ ③ BLOB STORE     content-addressed by sha256                   │
│                                                                │
│   blobs/sha256/<aa>/<full-hash>                                │
│   referenced from events by hash                               │
│   GC'd by reference count                                      │
│                                                                │
│   See content-addressable-storage plan for details             │
└───────────────────────────────────────────────────────────────┘
```

The existing per-session `meta.json` files under `namespaces/<name>/sessions/<id>/` are kept unchanged — current Tender code keeps working without modification.

## Why no transactional DB

Considered SQLite for live state. Rejected for Tender's scale and shape:

| Property | SQLite blocks.db | In-memory index + log |
|---|---|---|
| Files | events.jsonl + blocks.db + annotations.db + blobs/ | events.jsonl + blobs/ |
| New dependencies | rusqlite | none (std HashMap) |
| Schema migrations | 2 systems | 1 (JSONL `schema_version`) |
| Crash recovery | event log + DB consistency check | event log only |
| Point lookup | < 1 ms (indexed) | < 1 µs (HashMap) |
| List in namespace (1k results) | < 10 ms | < 1 ms |
| Code to maintain | ~500 lines | ~150 lines |

Tender is one daemon per host. Block counts are small (~10k/year for heavy users). Restart cost from log replay is ~50–100 ms for 100k events. SQLite earns nothing here.

DuckDB handles analytical queries (multi-namespace, time-range, joins, aggregation) without writes — see [event-log-analytics](../active/00_event-log-analytics-v1.md). No need for a transactional DB for analytics either.

## Event log schema

One JSON object per line. Fields:

| Field | Type | Description |
|---|---|---|
| `schema_version` | integer | Bumps explicit, documented in CHANGELOG |
| `event_id` | ULID string | Stable identity, sortable by time |
| `ts` | RFC3339 nanos | Wall-clock UTC |
| `monotonic_seq` | integer | Per-namespace strict total order |
| `namespace` | string | Routing key |
| `kind` | enum | `lifecycle | emitted | annotation | artifact-ref` |
| `type` | string | Subtype within kind (`lifecycle:exited`, `tool-start`, `tag-added`, …) |
| `block_id` | ULID string | Which block this event belongs to |
| `parent_block_id` | ULID string \| null | Causality link |
| `actor` | string | What emitted (`claude-code`, `tender`, …) |
| `host` | string | Stable host id |
| `payload` | JSON object | Type-specific; opaque to Tender |
| `payload_blob` | sha256 ref \| null | Set if payload exceeded 256 KiB inline cap |

Total order within a namespace is by `monotonic_seq`. Cross-namespace order is best-effort via `ts` (clocks may skew).

## In-memory state index

Built from the event log on daemon startup. The materialized view of every block's current state:

```rust
struct BlockState {
    block_id:        BlockId,
    namespace:       Namespace,
    parent:          Option<BlockId>,
    host:            HostId,
    spec:            CommandSpec,
    state:           LifecycleState,         // Queued | Running | Done | Failed | Canceled
    created_at:      Timestamp,
    started_at:      Option<Timestamp>,
    completed_at:    Option<Timestamp>,
    exit_code:       Option<i32>,
    exit_signal:     Option<i32>,
    stdin_sha256:    Option<Sha256>,
    stdout_sha256:   Option<Sha256>,
    stderr_sha256:   Option<Sha256>,
    provenance_hash: Option<Sha256>,
    tags:            BTreeSet<String>,
    annotations:     BTreeMap<String, JsonValue>,
}
```

Secondary indexes maintained alongside (each is a `HashMap` or `BTreeMap`):

- `by_namespace: HashMap<Namespace, BTreeSet<BlockId>>`
- `by_parent:    HashMap<BlockId, Vec<BlockId>>`
- `by_tag:       HashMap<String, BTreeSet<BlockId>>`
- `by_state:     HashMap<LifecycleState, BTreeSet<BlockId>>`

All indexes mutate on each event in O(log n) or O(1). Memory cost: ~1 KB per block. 100k blocks ≈ 100 MB. Fine.

## Checkpoint snapshots (optional, performance)

To avoid replaying the entire log on every restart, periodically write a compact binary snapshot of the in-memory state:

```
namespaces/<name>/snapshot.bin    (bincode-encoded BlockState records)
namespaces/<name>/snapshot.seq    (monotonic_seq at which snapshot was taken)
```

On startup:
1. Load `snapshot.bin` if present → populate in-memory state
2. Replay events from log starting at `snapshot.seq + 1`

Snapshots are never canonical. Delete the file at any time; the log will rebuild the state. Tunable: snapshot interval (e.g. every 10k events or every 5 minutes).

## Annotations & tags are events too

Tag and annotation mutations are recorded as events:

```json
{ "kind": "annotation", "type": "tag-added",     "payload": { "tag": "wip" } }
{ "kind": "annotation", "type": "tag-removed",   "payload": { "tag": "wip" } }
{ "kind": "annotation", "type": "kv-set",        "payload": { "key": "owner", "value": "rick" } }
{ "kind": "annotation", "type": "kv-removed",    "payload": { "key": "owner" } }
```

In-memory state index updates on each. Audit trail is automatic — the log shows when every tag was added/removed and by whom.

## Ordering & consistency guarantees

1. **Within a namespace**, events are totally ordered by `monotonic_seq`. Strictly increasing per namespace.
2. **Across namespaces**, only `ts` provides ordering. Clocks may skew between hosts.
3. **Append-only writes** to `events.jsonl` use `O_APPEND` + a single `write()` of the JSON line + newline. POSIX guarantees atomicity for writes under PIPE_BUF (usually 4 KiB); we cap event size at 256 KiB so writes use record-level locking + retry where atomicity isn't guaranteed by the kernel.
4. **Reader semantics**: tail the JSONL with inotify/kqueue/ReadDirectoryChangesW; emit lines as written. Late readers can replay from any byte offset.
5. **Crash safety**: fsync on every event by default. Opt out via `--fast-events` if throughput matters more than per-event durability (still safe; loses only events from the past few ms on crash).

## Schema versioning

`schema_version` is a single integer on every event. Bumps require:

1. A migration script under `schema/migrations/<n>-to-<n+1>.{sql,rs}`
2. A CHANGELOG entry
3. A test fixture proving forward conversion preserves semantics
4. Documentation of breaking-vs-additive changes

**Rule**: Tender accepts events with `schema_version <= current`. Newer events from a fresher Tender process are rejected by older readers (forward compatibility is opt-in). Same shape as [boundary-metadata](../active/01_boundary-metadata.md)'s discipline.

## Storage scaling shape

| Scale | Event log | Index | Blobs |
|---|---|---|---|
| **Single user** | JSONL + occasional Parquet rollover | in-memory HashMap | local filesystem |
| **Heavy use** | JSONL + daily Parquet rollover + `tender prune` | in-memory + snapshot checkpoints | local filesystem |
| **Fleet / cloud** | Parquet in object storage; DuckDB reads via `httpfs` | per-host in-memory | S3-compatible object storage |

Tender's first delivery is single-machine; later backends are opt-in. Same interfaces.

## What this design rejects

- ❌ SQLite or any other transactional database for state
- ❌ Bespoke index files (e.g. B-trees on disk) — DuckDB does this when needed
- ❌ Two sources of truth (log + DB that can disagree)
- ❌ Schema migrations on a relational store with FK constraints
- ❌ Database file corruption recovery (events.jsonl is plain text)
- ❌ Locked-in storage format — JSONL and Parquet are both portable

## Open questions

1. **Checkpoint snapshot format.** Bincode is fast and small but Rust-specific. CBOR is portable but larger. JSON is huge. Recommend bincode for v1; revisit if non-Rust tools want to read snapshots (they shouldn't — they'd replay the log).
2. **Snapshot cadence.** Every N events vs every M minutes vs both. Recommend both with conservative defaults; user-tunable.
3. **Atomic multi-event writes.** If a state transition involves multiple events (e.g. block exit + on_exit callback fire + annotation), do we need transactional semantics? Recommend no — each event is independent and order-stable; consumers reason about state from the event stream's order.
4. **Reader catch-up after disconnect.** Subscribers with persistent state (e.g. a dashboard) need to know "where I left off." Recommend exposing `monotonic_seq` in `tender watch` output so consumers can checkpoint.
5. **Multi-writer on one host.** Use file locking (`flock`) per namespace; single-writer guarantee. Concurrent reads are fine.
6. **Encryption at rest.** Rely on OS / full-disk encryption. Tender doesn't encrypt in-process. Document the trust boundary.
7. **Federation across hosts.** A block on host A might reference a parent on host B. Out of scope for this plan; opaque ID references. Federation = a separate plan when needed.

## How this composes with existing backlog

| Plan | Relationship |
|---|---|
| [content-addressable-storage](../backlog/content-addressable-storage.md) | Implements ③ blob store + provenance hashing |
| [event-log-analytics](../active/00_event-log-analytics-v1.md) | DuckDB read-side over ① event log; no writes through DuckDB |
| [boundary-metadata](../active/01_boundary-metadata.md) | Adds `boundary_kind` / `boundary_label` as event fields; reflected in in-memory index |
| [provenance-on-lifecycle-transitions](../completed/2026-04-16-provenance-on-lifecycle-transitions.md) | Adds `transition_provenance` as event field |
| event-emit-primitive (active) | Defines the JSONL schema and `tender event emit` writer |

No existing plan is invalidated. The persistence model knits them together.

## Implementation order (sketch)

These would be sliced as concrete backlog items if/when work begins:

1. **JSONL writer + in-memory index** — the foundation. Replaces the implicit "live state lives in process memory" with an explicit log-backed model.
2. **Snapshot/restore** — checkpoint mechanism for fast restart.
3. **`tender event emit`** — already an active plan (covers the writer surface for hooks).
4. **CAS blob store** — separate plan, separate work.
5. **Parquet rollover + DuckDB query** — separate plan, fully composable on top.

Existing per-session `meta.json` continues to work throughout; this work is additive.
