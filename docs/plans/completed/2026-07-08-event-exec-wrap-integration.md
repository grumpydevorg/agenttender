---
id: event-exec-wrap-integration
depends_on: []
links:
  - ../specs/event-protocol.md
  - 2026-07-07-event-emit-primitive.md
  - 2026-07-07-event-follow-cursors.md
  - ../backlog/boo-integration.md
---

# Event Protocol Slice 3 — exec/wrap integration, ambient causality

**Shipped 2026-07-08** via PR #10 (main@7c91532). All acceptance criteria
below are test-covered (`cli_exec`, `cli_wrap`, `cli_emit`, `cli_on_exit`,
`cli_pty`, `events_log`, and the slice-3 causal-tree/spill/env-unset
scenarios in `acceptance_event_protocol` — 563 tests green, 535 baseline
+ 28 new; best-effort proven via read-only `events/` dirs). Shipped as
planned for `emit`, `exec`, and the sidecar facts (`callback.finished`,
`pty.control_changed`), with **one intentional compatibility carve-out**
on wrap's `--event` validation — see the shipped-deviation section below.

## Shipped deviation — wrap `--event` compatibility carve-out

The plan pinned `--event` validation via `Kind::new_user` AND "cli_wrap
passes unmodified" (acceptance criteria) — but the kind grammar requires
a dotted name and wrap's pre-existing surface uses dotless names
(`test-event`, `pre-tool-use`), so strict validation could not satisfy
both pins. Shipped behavior:

- **Dotted protocol-valid kinds dual-write**: stored event (spec example
  (b) data shape) + A-line linked by `event_id`/`block_id`, child env
  chain set.
- **Reserved kinds still exit 6 before any side effect** — child never
  spawned, nothing written anywhere. Precisely reserved *kinds*: names
  that parse as kinds under a reserved prefix. A grammar-invalid name is
  not a kind and takes the legacy path below (it can never produce a
  stored event, so the reserved namespace is not at risk).
- **Legacy dotless names keep the exact pre-slice-3 behavior**:
  annotation-only — no stored event, no child env chain — plus a stderr
  notice, which fires on every legacy invocation (deliberate migration
  nudge; the notice text is not a pinned surface).

Consumer caveat (inherent to pre-minting + best-effort, accepted): wrap
exports `TENDER_PARENT_EVENT_ID` — the id of the event it *will* write —
before spawning the child, so if wrap's append later fails (e.g.
unwritable `events/`), child-emitted events carry a `parent_id` that
resolves to no stored event and `parent_id → id` joins drop those edges.
The A-line still records `block_id` (without `event_id`) in that case.

---

Implements slice 3 of [specs/event-protocol.md](../specs/event-protocol.md)
(the schema owner — envelope/storage/ordering decisions live there, not
here): the write-side integration. `exec` and `wrap` become event producers
(§6: "sugar over the same append"), `TENDER_BLOCK_ID` /
`TENDER_PARENT_EVENT_ID` make causality ambient (§2), and the sidecar gains
the two remaining durable facts: `callback.finished` and
`pty.control_changed`. Slice 1 (envelope, append, WAL lifecycle, emit,
replay) shipped 2026-07-07 via PR #4; slice 2 (follow, cursors, re-backed
watch) shipped 2026-07-07 via PR #7.

The read side needs zero changes: `tender events --kind exec.` works on day
one (prefix filters shipped in slice 1), and watch's frozen output shape is
untouched. This slice is producers only.

## One chaining rule, three producers

Every producer defaults its causal fields from the same environment chain:

- `block_id` ← `TENDER_BLOCK_ID` when the producer didn't mint its own.
- `parent_id` ← explicit `--parent` > `TENDER_PARENT_EVENT_ID` >
  `TENDER_BLOCK_ID`.

`emit` applies the rule as-is. `exec` and `wrap` mint a fresh `block_id`
per invocation (stamped on their own events) and apply the rule to
`parent_id` — so a wrap running inside an exec block chains to it, and a
hook-spawned `emit` chains to the hook event, with no flags anywhere.
Malformed env values warn on stderr and are ignored — ambient env must
never hard-fail a producer.

## Scope (slice 3 only)

1. **`exec.started` / `exec.result`** (`src/commands/exec.rs`) — `cmd_exec`
   mints a `block_id` (UUIDv7) after acquiring the exec lock, opens one
   `EventWriter::new` (both events share the writer, `seq` 1 and 2), and
   emits with `source = Source::trusted("tender.exec")`, `kind` via
   `Kind::new` (`exec.` is a reserved prefix — internal call sites are the
   only permitted writers, spec §1):
   - `exec.started` before frame injection, `data:
     {"command": [argv…], "exec_target": "<the target run_exec dispatches
     on>", "timeout_ms": <when set>}`.
   - `exec.result` after the wait completes, `data: {"exit_code",
     "cwd_after", "timed_out", "stderr", "stdout", "truncated": false}` —
     the shipped `ExecResult` fields minus `session` (the envelope carries
     it). Oversize spill per §3.4 with a **structured preview** (item 7):
     spec example (d)'s shape — structured fields intact, `stdout` replaced
     by a truncated `stdout_preview`, `truncated: true`.
   - Timeout still emits `exec.result` (`timed_out: true`, partial output);
     the exit-124 path is unchanged.
   - `generation` from meta when known. Both events set `parent_id` from
     exec's own env chain (nested producers chain upward).
   - The exec A-line stays, gaining additive `event_id` (the `exec.result`
     event's id) and `block_id` fields — same linkage contract as wrap
     (§0). The JSON stdout envelope (`ExecResult`) is frozen: no field
     changes.
2. **PosixShell frame propagation** (`src/exec_frame.rs`) — `unix_frame`
   gains the block id: `export TENDER_BLOCK_ID='<uuid>'; {cmd};
   __tender_s=$?; unset TENDER_BLOCK_ID; printf …` — set before the
   payload, captured exit code first, unset before the sentinel so the
   session shell isn't left polluted. A payload that runs `tender emit`
   lands in the exec block automatically. PosixShell only this slice
   (see decisions).
3. **wrap dual-write** (`src/commands/wrap.rs`) — per spec §0/§6:
   - `--event` validated through `Kind::new_user` **before any side
     effect**: reserved prefix ⇒ exit 6, child not spawned, nothing
     written (aligns with emit's exit-code table; `--source` is already
     validated at the clap boundary).
   - Pre-mint the event id and a `block_id` before spawn; the child env
     (currently the empty `BTreeMap` passed to `spawn_child`) gains
     `TENDER_BLOCK_ID` = the minted block and `TENDER_PARENT_EVENT_ID` =
     the pre-minted event id — "the id of the event it *will* write" (§2).
   - After the child exits: append the event — `kind` = `--event`,
     `source` = `--source`, `run_id` from `TENDER_RUN_ID` (the existing
     hard requirement), `generation` from `TENDER_GENERATION` when set,
     `block_id` = minted, `parent_id` from wrap's own env chain, `data` =
     the shipped hook shape verbatim (`hook_stdin`, `hook_stdout`,
     `hook_stderr`, `hook_exit_code`, `command`, `truncated`) — spec
     example (b). Oversize rides the generic §3.4 spill.
   - Then write the A-line via the existing three-tier
     `build_annotation_payloads` fallback, with additive `event_id` and
     `block_id` fields. Event before A-line: the event is authoritative,
     the A-line is projection.
   - The child's exit code passes through unconditionally.
4. **emit env defaults** (`src/commands/emit.rs`) — apply the chaining
   rule: `block_id` from `TENDER_BLOCK_ID`; `parent_id` from `--parent` >
   `TENDER_PARENT_EVENT_ID` > `TENDER_BLOCK_ID`. Explicit `--parent` that
   fails to parse stays exit 2; malformed *env* values warn + ignore.
   This un-inerts the default documented since slice 1 (§6).
5. **`callback.finished`** (`src/sidecar.rs`, on-exit loop) — one event
   per configured callback, emitted as each finishes, through the
   in-scope lifecycle `EventWriter` (run-id writer identity, `seq` stays
   contiguous after the terminal transition). `kind` via `Kind::new`
   (reserved), `source tender.sidecar`, `data: {"index", "command",
   "status", "exit_code"?, "stderr"?, "error"?}` — the per-callback record
   shape and its `ok` / `failed` / `spawn_failed` vocabulary verbatim.
   The `callbacks/{run_id}.json` batch file is unchanged.
6. **`pty.control_changed`** (`src/sidecar.rs`, attach-server loop) — at
   the two `update_pty_control` call sites, emitted **before** the meta
   control write (§3.6 WAL order): attach ⇒ `data: {"control":
   "HumanControl", "trigger": "attach"}`, detach ⇒ `{"control":
   "AgentControl", "trigger": "detach"}` — the shipped `PtyControl`
   vocabulary verbatim, nothing else. The attach-server thread owns its
   own `EventWriter::new` (fresh writer identity, own `seq` chain — the
   protocol is multi-writer by design; no sharing with the lifecycle
   writer across threads).
7. **Shared core** (`src/events.rs`) — two additive `EventDraft`
   extensions, both `Option` so every existing call site is untouched:
   - `id: Option<Uuid7>` — caller-supplied pre-minted id (`None` = stamp
     at append, as today). Only wrap uses it.
   - `preview: Option<serde_json::Value>` — caller-supplied structured
     spill preview, used instead of the generic head-of-JSON `preview_of`
     when `data` spills; if the supplied preview itself exceeds
     `MAX_PREVIEW_BYTES`, degrade to the generic preview. Only exec uses
     it.

## Tender/Boo boundary (design note)

Tender records **durable execution and control facts**; it does not do
rendered-screen automation. `pty.control_changed` is a control fact — who
owns the PTY's input — not a screen-state event: no screen contents, no
cursor position, no "output settled" semantics. Screen-state verbs
(`send` / `peek` / `wait`-for-pattern) belong to a Boo/Ghostty adapter
([backlog/boo-integration.md](../backlog/boo-integration.md)) or a future
PTY integration layer, which would *report through* this protocol — its
own kinds (`boo.*` — the vocabulary is open, §1) via `emit`/`wrap`, its
consumption via `events --follow`. Slice 3 deliberately ships only the
protocol hooks such an adapter needs, and `pty.control_changed` stays
minimal so the boundary holds. If richer terminal semantics start creeping
into this slice, they move to an adapter slice instead.

## Decisions pinned here (implementation-level; spec stays authoritative)

- Event emission from exec and wrap is **best-effort**: append failure
  warns on stderr and never alters the command's exit code or output
  (exec's JSON envelope, wrap's exit passthrough). Sidecar
  `callback.finished` / `pty.control_changed` emission is likewise
  best-effort — these are non-terminal facts; the terminal-transition
  lost+found salvage (§3.6) is not extended to them.
- Env propagation into exec frames is **PosixShell only** this slice.
  PowerShell/Python frames could carry it (`$env:` / `os.environ`) but
  their payloads spawning `tender` is speculative — deferred until a
  consumer demands it (the house gate, cf. the `--replace` carry-forward).
  DuckDB structurally can't. The spec's §2 "set by exec/wrap" is
  reconciled to the shipped surface at archive time.
- `exec.started` and `exec.result` are **siblings within the block**
  (shared `block_id`); result does not set `parent_id` to started —
  matches spec example (d).
- wrap's kind validation is argument validation (spec §1): it precedes
  the spawn, so a reserved `--event` is a config error surfaced loudly at
  exit 6, not a half-run hook.
- A-line additions (`event_id`, `block_id`) are additive-only; watch's
  `--annotations` projection passes them through inside `data` with its
  own record shape untouched (frozen, §5.3). Consumers ignore unknown
  fields — envelope doctrine.
- No provenance field on any slice-3 event: all are direct facts recorded
  by the process that performed them.

## Non-goals (later slices, per spec §11)

- Rotation, `segment.opened`, `--events-keep-segments`, prune sweep
  additions, `--replace` carry-forward (slice 4).
- `events` in `REMOTE_COMMANDS`, Windows CI for the event suite, notify
  wake-up hints (slice 5).
- Frame env propagation for PowerShell/PythonRepl/DuckDb targets
  (deferred, see decisions).
- Screen-state automation (`send`/`peek`/`wait`), block extraction
  commands, any rendered-terminal semantics — adapter territory (boundary
  note above).

## Validation

Canonical example — the causal tree, rebuilt by its three foreign keys
(spec §2). A Claude Code hook wrapped by tender, where the hook script
itself emits:

```sh
tender wrap --source claude.hook --event hook.post_tool_use -- ./post.sh
# post.sh contains: tender emit --kind hook.note --data '{"note":"…"}' --best-effort
```

yields two stored events: the hook event (block_id = wrap's block) and the
note event with `parent_id` = the hook event's id and `block_id` = wrap's
block — plus the A-line carrying `event_id`. And under exec:

```sh
tender exec sh-session -- sh -c 'tender emit --kind build.step --data "{}"'
```

yields `exec.started` → `exec.result` sharing a `block_id` that the
`build.step` event also carries (frame propagation). One DuckDB pass over
`events/*.jsonl` groups by `run_id`/`block_id` and joins `parent_id` → `id`
to print the tree.

## Acceptance criteria

- `tender exec` on a live PosixShell session emits `exec.started` +
  `exec.result` sharing a `block_id`, writer-contiguous `seq`, with the
  pinned data shapes; `tender events --kind exec.` returns both with no
  reader changes. The exec JSON stdout envelope is byte-identical in field
  set to before; exit codes (0 / inner / 124) unchanged.
- An exec payload running `tender emit` produces an event whose `block_id`
  equals the exec block; after the block, `TENDER_BLOCK_ID` is unset in
  the session shell — probed via a raw `tender push` of
  `echo ${TENDER_BLOCK_ID:-unset}` (a follow-up exec can't probe this: it
  exports its own).
- `exec` with ≥1 MiB stdout: `exec.result` carries a valid
  `data_ref.sha256` blob of the full data, and the inline `data` is the
  structured preview — `exit_code`, `cwd_after`, `timed_out` still
  queryable inline, `truncated: true`.
- Exec timeout: `exec.result` present with `timed_out: true`; process
  still exits 124.
- `tender wrap` dual-writes: stored event (spec example (b) data shape)
  and A-line whose `event_id` equals the event's id; the child sees
  `TENDER_BLOCK_ID` and `TENDER_PARENT_EVENT_ID` == that same event id
  (asserted via an env-dumping fixture); child exit code passes through
  including on event-append failure (best-effort proven by a read-only
  `events/` dir).
- The validation scenario's causal tree: hook event ← note event via
  `parent_id`, both in wrap's block — rebuilt by one DuckDB
  `read_json` pass over the segment files.
- `tender wrap --event run.hijack …` exits 6; no child ran, no event, no
  A-line.
- `emit` precedence: with both env vars set, `parent_id` =
  `TENDER_PARENT_EVENT_ID`; explicit `--parent` beats both; `block_id` =
  `TENDER_BLOCK_ID`; a malformed `TENDER_BLOCK_ID` warns and is ignored,
  exit 0.
- A session with two `--on-exit` callbacks (one failing) replays
  `run.exited` then two `callback.finished` events — same writer id,
  contiguous `seq`, statuses `ok` and `failed` with `exit_code` — and
  `callbacks/{run_id}.json` is unchanged in shape.
- PTY attach then detach emits `pty.control_changed`
  (`HumanControl`/`attach` then `AgentControl`/`detach`), each appended
  before the corresponding meta `pty.control` flip; data carries exactly
  the two pinned fields.
- All existing tests stay green: `cli_exec`, `cli_wrap`, `cli_watch`,
  `cli_events_*` pass unmodified; meta.json and output.log O/E lines
  byte-identical; A-line changes additive-only.
