---
id: event-emit-primitive
depends_on: []
links:
  - ../specs/event-protocol.md
  - ../specs/tender-as-block-runtime.md
---

# Event Protocol Slice 1 — emit, lifecycle events, replay

**Shipped 2026-07-07** via PR #4 (main@6992769). All acceptance criteria
below are test-covered (`tests/acceptance_event_protocol.rs` and the
`cli_events_*`/`cli_emit`/`events_log`/`model_event` suites). One grammar
decision made during implementation: kind grammar shipped as the `Source`
grammar **plus `_`** — see spec §1. Slices 2–5 remain unscheduled.

Implements slice 1 of [specs/event-protocol.md](../specs/event-protocol.md)
(the schema owner — all envelope/storage/ordering decisions live there, not
here). Daemonless: every event is one O_APPEND JSONL line in the session's
`events/` dir. No `TENDER_SOCKET`, no bus, no new dependencies.

Rewritten 2026-07-06. The previous version of this plan (daemon ingest,
socket transport, ULID ids, reject-oversize) is superseded by the spec's
§10 reconciliation table; its `powershell-exec-side-channel` dependency is
already shipped and dropped.

## Scope (slice 1 only)

1. **Envelope type** — `Event` struct per spec §1: serde round-trip, kind
   grammar validation (reuse `Source` grammar), UUIDv7 ids via the shipped
   machinery, RFC 3339 µs `Z` timestamps.
2. **Append path** — spec §3.2: newest-segment pick / `create_new` race
   handling, `append(true)` open, advisory `flock` on `events/append.lock`
   (POSIX only; Windows relies on the documented `FILE_APPEND_DATA`
   contract), single `write_all`, 16 KiB data cap with sha256-keyed blob
   spill (§3.4), inline-truncate degradation on blob failure.
3. **Sidecar lifecycle events** — at every `write_meta_atomic` transition
   site, WAL order (spec §3.6): event append (fdatasync for terminal
   transitions) **before** meta write. Kinds = shipped watch vocabulary
   (`run.starting` … `run.dependency_failed`) with watch's `data` shapes.
   The CLI reconciliation path (`wait`/`status`/`run`) appends the inferred
   `run.sidecar_lost` event with `data.provenance:"inferred"`, and checks
   the event-log tail first (`Evidence::EventLogTerminal`) to heal meta
   from a sidecar-written terminal event when one exists.
4. **`tender emit`** — spec §6: flags, granular exit codes (0/2/3/5/6),
   `--best-effort`, `--durable`, lost+found fallback for orphaned emitters
   (spec §7). Slice-1 causality is `run_id` + explicit `--parent` only;
   `--source` defaults to `user.emit`. Nothing sets `TENDER_BLOCK_ID`
   until slice 3, so the env-var default is inert here by design.
5. **`tender events` (replay only)** — read all segments of matching
   sessions, merge by `(ts, writer, seq)`, envelope NDJSON to stdout;
   `--kind`/`--source`/`--session`/`--namespace` filters; `--strict`
   (parse-skips ⇒ exit 65). No `--follow`, no cursors (slice 2).
6. **Env** — export nothing new from the sidecar; `TENDER_BLOCK_ID` /
   `TENDER_PARENT_EVENT_ID` arrive with slice 3 (exec/wrap sugar).

## Non-goals (later slices, per spec §11)

- Follow mode, cursor tokens, bookmarks, cursor-gone (slice 2).
- exec/wrap integration, `TENDER_BLOCK_ID`, `pty.control_changed`,
  `callback.finished` (slice 3).
- Rotation, prune additions, `--replace` carry-forward decision (slice 4).
- `REMOTE_COMMANDS` entry, Windows CI, notify hints (slice 5).
- Re-backing `tender watch` (slice 2, after cursors).

## Validation

Canonical example: a Claude Code PostToolUse hook running

```sh
tender emit --kind hook.post_tool_use --source claude.hook \
            --data-stdin --best-effort
```

inside a supervised session (`hook.` is deliberately unreserved — spec §1),
then `tender events --kind hook.` replaying it with correct
`run_id`/`source` and occurrence-time timestamps.

## Acceptance criteria

- `kill -9` a supervised run; after reboot, `tender events` replays
  `run.starting → run.started → run.sidecar_lost` with occurrence-time
  timestamps and `provenance:"inferred"` on the last.
- A terminal transition (`run.exited`) present in meta is **always**
  present in the event log (WAL invariant), verified by a crash-injection
  test between the two writes.
- Two concurrent `tender emit` processes × 1000 events each: zero torn or
  interleaved lines (POSIX: flock; Windows: append contract), all 2000
  present, per-writer `seq` contiguous.
- An emit with 1 MiB `data` produces a blob + preview event with
  `truncated:true` and a valid `data_ref.sha256`; identical payload emitted
  twice stores one blob.
- `tender emit --best-effort` in a pruned session exits 0 and the event is
  recoverable from `~/.tender/lost+found/events.jsonl`.
- DuckDB `read_json('~/.tender/sessions/<ns>/<s>/events/*.jsonl')` returns
  typed rows with zero schema wrangling.
- All existing tests stay green; `output.log`, meta.json, watch output, and
  exec envelopes byte-identical to before.
