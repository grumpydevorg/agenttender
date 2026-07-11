---
id: content-addressable-storage
depends_on:
  - event-emit-primitive
links:
  - ../specs/event-protocol.md
  - ../specs/tender-as-block-runtime.md
---

# Content-Addressable Storage Extensions

> **Status: consumer-gated.** The useful primitive already shipped in the
> event protocol: oversized event payloads spill into per-session
> `events/blobs/<sha256>` and are exposed as `data_ref`. Session-local retention
> remains owned by `prune`. Do not build a global CAS, refcounts, replay cache,
> or new `block` record vocabulary speculatively.

## Reconsideration trigger

Reopen this work only when a named consumer requires at least one capability
that the shipped per-session blob store cannot provide:

- a reproducible export/crash bundle with verified payload hashes;
- a new captured-artifact class that must use `data_ref`;
- measured storage pressure where cross-session deduplication has material
  value; or
- retention/garbage-collection behavior that `prune` cannot express.

The consumer must supply representative data and an ownership model for
retention. Cross-session deduplication is not a local layout tweak: it breaks
the current rule that deleting a session deletes all of its events and blobs.

## Constraints that survive the older design

- The event envelope and `data_ref` schema remain owned by
  [event-protocol.md](../specs/event-protocol.md).
- Blob keys are SHA-256 of the exact serialized payload bytes.
- Writes remain temp-file plus atomic rename; readers verify size/hash when
  integrity matters.
- Existing session directories and event readers remain compatible.
- No daemon, global block index, hidden database, or cache-driven execution.
- Until a consumer proves otherwise, session-local blobs plus `prune` are the
  complete design.

The previous global-CAS, ULID/`parent_block_id`, daemon, refcount, and
`tender block output` sketches were superseded by the shipped event protocol
and remain available in git history if a future consumer needs their rationale.
