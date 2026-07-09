---
id: boundary-metadata
depends_on: []
links: []
---

# Boundary Metadata — Describe Where a Session Runs

Add an optional boundary descriptor to sessions so `status`, `watch`, and a future `graph` command can show where sessions run without managing those environments.

## Why

Tender sessions can run on the local host, inside Docker containers, on remote hosts via `--host`, or inside VMs. Today nothing in `meta.json` records which. A user running `tender list` across namespaces has no way to distinguish a local session from one that lives inside a container on a remote box.

This is a legibility problem, not a control problem. Tender should describe boundaries, not manage them.

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

## Scope

- additive metadata only
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
- no existing behavior changes
