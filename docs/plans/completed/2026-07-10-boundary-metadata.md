---
id: boundary-metadata
depends_on: []
links:
  - ../specs/event-protocol.md
---

# Boundary Metadata — Describe Where a Session Runs

> **Shipped 2026-07-10 via PR #27 at main@`d3da227`.**
> `LaunchSpec.boundary: Option<BoundaryContext>` (host/container/vm/pod +
> label) is the current-state authority in `meta.json`; an immutable
> `data.boundary` snapshot rides `run.starting` / `run.started` **only**
> (terminal and inferred events join back on `run_id`) as the history
> authority. CLI: `--boundary KIND:LABEL` (first-colon split, so labels keep
> tag colons like `my-image:latest`) and repeatable `--boundary-parent`; both
> survive `--host` arg reconstruction. `status` surfaces it through the existing
> meta-JSON output; old `meta.json` deserializes with `boundary: None`, and an
> absent boundary is omitted so canonical hashes stay stable. Covered by
> `model_boundary`, `cli_boundary`, `model_spec`, and `cli_remote` tests; full
> suite green. Plan archival follows the implementation PR by design.
>
> **Scope note:** the optional `boundary_kind` / `boundary_label` query
> convenience columns remain the documented *later nicety* (see "Querying by
> boundary" below) — **not** part of this slice. Strong-v1 boundary analytics
> is the snapshot / `run_id` join, which needs no `tender query` change.

Add an optional boundary descriptor to sessions so `status`, `watch`, and a future `graph` command can show where sessions run without managing those environments.

## Why

Tender sessions can run on the local host, inside Docker containers, on remote hosts via `--host`, or inside VMs. Today nothing in `meta.json` records which. A user running `tender list` across namespaces has no way to distinguish a local session from one that lives inside a container on a remote box.

This is a legibility problem, not a control problem. Tender should describe boundaries, not manage them.

## Authority vs. history

> Boundary metadata is authoritative in `LaunchSpec` / `meta.json`; lifecycle
> events carry a denormalized immutable snapshot for historical analytics. The
> event snapshot is derived from launch metadata, not independently edited.

This keeps Tender's existing split intact — `meta.json` is the current-state
authority, `events/` is the history authority — and gives each question exactly
one owner:

| Question | Owner |
|---|---|
| What boundary was this session launched with? What should `status` show? How should old/new `meta.json` deserialize? What did the user declare (without Tender managing Docker/k8s)? | **`LaunchSpec.boundary` / `meta.json`** — current-state authority |
| Where was this run when it happened? How do I query historical failures by host/container? What boundary was true for a past run, independent of later `meta.json` edits? | **The lifecycle-event snapshot** in `events/` — history authority, within retained event history |

Analytics must read the boundary snapshot that was true when the run was
recorded. In particular, **`tender query` must not join *current* `meta.json` to
*old* events as the default** — that would lie historically, because a session
can be replaced, moved, or re-declared with a different boundary after the fact.

## Design

### Boundary types

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BoundaryKind {
    Host,
    Container,
    Vm,
    Pod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boundary {
    pub kind: BoundaryKind,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryContext {
    pub current: Boundary,
    pub parents: Vec<Boundary>,
}
```

Flat ancestry vector, not recursive `Box<Boundary>` — simpler serde, simpler diffs, simpler partial updates.

### Where it lives

The boundary is part of the declared execution context, so it belongs on `LaunchSpec`:

```rust
pub struct LaunchSpec {
    // ... existing fields ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<BoundaryContext>,
}
```

If Tender later infers or enriches boundary information (e.g., detecting it's running inside a container), that observed value can be mirrored into `Meta` with appropriate provenance. For now, the boundary is user-supplied and declared.

### The history snapshot

At run start, the declared boundary is stamped — denormalized and immutable —
into the lifecycle events as `data.boundary`, on `run.starting` / `run.started`.
This is a copy *derived from* `LaunchSpec.boundary`, not an independently
editable field: the launch path writes it once and nothing mutates it afterward.
It is the boundary that historical analytics query — the boundary that was true
for a run stays readable from the event log independent of any later `meta.json`
edit.

**Retention caveat.** The snapshot lives inside the run's event history, so it
lasts exactly as long as that history is retained — no longer. Under current
semantics, `--replace` removes the old session directory (events and all) and
`prune` deletes terminated sessions; both take the events, and the snapshot with
them. `--replace` event carry-forward is deliberately deferred (see
[event-protocol.md](../specs/event-protocol.md)); if it lands, the boundary
snapshot moves with the carried-forward old events. So the snapshot is the
history authority **within retained event history**, not a promise of survival
across current `--replace` / `prune` deletion.

### CLI surface

```bash
tender start job --boundary host:data-box -- make test
tender start dev --boundary container:my-image:latest --boundary-parent host:data-box -- bash
```

The `--boundary` flag takes `kind:label`. Optional `--boundary-parent` adds ancestry entries. Both are omitted by default — existing sessions work unchanged.

### Surfaced in

- `tender status` — shows boundary context if present
- `tender list` — optionally group/filter by boundary
- future `tender graph` — render sessions within their boundaries

### Querying by boundary (analytics)

Because the snapshot lives on the run's lifecycle events, boundary analytics
join other events to their run's lifecycle event on `run_id`:

- **Strong v1** — query by boundary via the lifecycle-event snapshot, or join
  other events on `run_id` to pick up `data.boundary`. No change to
  `tender query` is required; the snapshot is queryable the moment it is emitted.
- **Later nicety** — `tender query` may project convenience columns
  (`boundary_kind`, `boundary_label`) into the `events` view via a query helper,
  *if* the join pattern proves common enough to earn it. Not v1.

## Scope

- additive metadata only
- `LaunchSpec` / `meta.json` is the boundary authority; lifecycle events
  (`run.starting` / `run.started`) carry an immutable `data.boundary` snapshot
  derived from it (never independently edited)
- no behavior change
- no Docker/Podman/k8s API integration
- no remote IPC model change
- no cross-container session sharing semantics
- no lifecycle management of the boundary environment

## Non-goals

- detecting the current boundary automatically (deferred — could use cgroup inspection, `/proc/1/cgroup`, or `/.dockerenv` heuristics later)
- managing the boundary environment (starting/stopping containers)
- routing commands to boundaries (that's `--host` for SSH, not a boundary feature)

## Acceptance criteria

- `LaunchSpec` carries an optional `BoundaryContext`
- boundary metadata round-trips through `meta.json`
- `tender status` includes boundary info when present
- old `meta.json` files without boundary deserialize cleanly (Option::None)
- when a session declares a boundary, its `run.starting` / `run.started`
  lifecycle events carry an immutable `data.boundary` snapshot derived from
  `LaunchSpec.boundary`
- the snapshot is immutable and independent of later `meta.json` edits, within
  retained event history (historical analytics group past events by boundary via
  `run_id`, not by joining current `meta.json`); it is **not** carried across
  current `--replace` / `prune` deletion — that is the deferred `--replace`
  carry-forward, out of scope for this slice
- no existing behavior changes
