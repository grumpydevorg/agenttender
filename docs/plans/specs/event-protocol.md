---
id: event-protocol
depends_on: []
links:
  - ./tender-agent-process-sitter.md
  - ./tender-as-block-runtime.md
  - ./persistence-architecture.md
  - ../completed/2026-07-07-event-emit-primitive.md
  - ./ecosystem-landscape.md
---

# Tender Event Protocol v1 — daemonless, files-first

**Status: adopted design, schema owner.** This spec is the single source of
truth for tender's structured event stream — "the protocol layer between
supervised execution and any presentation." It supersedes the event schemas
in `01_event-emit-primitive.md` (pre-2026-07 version), the event/persistence
sections of `persistence-architecture.md`, and the envelope in
`hermes-block-runtime-integration.md` (§10 maps their fields here). Where
those documents disagree with this one, this one wins.

Provenance: produced 2026-07-06 from a judged three-design bake-off
(files-first vs sidecar-owned single-writer vs per-host bus), grounded in a
source-level map of tender's five shipped record schemas and prior art
(Kubernetes watch, journald, s6, supervisord, OSC 133, tmux control mode,
CloudEvents, OTel). The per-host bus lost on every lens; its steelman and
the judge scorecards are preserved in the session archive. Load-bearing
systems claims below were adversarially verified against primary sources.

## 0. Decisions (each previously open, now closed)

| Question | Decision |
|---|---|
| Daemon? | **No.** No resident process, no `TENDER_SOCKET` (that env var is rejected doctrine). Files are the transport; consumers read files. Consistent with the accepted process-sitter spec. |
| Identity | **UUIDv7 everywhere** (`uuid` crate, already a dep; the shipped `RunId` machinery). ULID and UUIDv4 variants in older docs are corrected; no mapping layer. |
| Timestamp | **RFC 3339 UTC, exactly 6 fractional digits, `Z`** — fixed-width, lexicographically sortable, DuckDB auto-casts. Stamped at occurrence time by the writer. `output.log`'s f64 stays (separate contract). |
| Oversize payloads | **Spill to blob, never reject.** The old plan's reject->256 KiB loses data. |
| Retention | **`prune` owns it, full stop.** Events live inside the session dir and ride existing deletion. No namespace-level files, no TTL daemon. |
| Schema authority | This spec. `kind` is an **open vocabulary** (CloudEvents-style), not a closed enum. |
| `watch` | Kept, output shape **frozen**, internally re-backed by the event log. `tender events` is the new protocol surface. |
| `wrap` | Kept as sugar + projection: dual-writes the authoritative event and its output.log A-line (linked by `event_id`). |

## 1. Envelope

One JSON object per line, one `write()` per line, `\n`-terminated. Fields
outside `data` are stamped by the tender binary (trusted tier), never copied
from user input. Hard line cap **32 KiB**; `data` cap **16 KiB** inline.

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int `1` | yes | Per-record envelope version. Additive fields never bump it. Consumers MUST ignore unknown fields and tolerate unknown `kind`s. |
| `id` | UUIDv7 | yes | Event identity + dedupe key. |
| `ts` | RFC 3339 µs `Z` | yes | Occurrence time, stamped by the writer at write time — never poll-detection time. |
| `kind` | dotted string ≤128B | yes | Routing + payload-schema id. Grammar = the shipped `Source` grammar (ids.rs) **plus `_`** — this spec's own worked examples use `hook.post_tool_use`, and slice 1 shipped that grammar (`Kind` in model/event.rs). Prefixes `run. log. exec. session. pty. callback. segment. cursor. tender.` are reserved to kinds whose payload schema tender itself owns. Rejection is enforced at **argument validation of user-supplied kinds** — `emit --kind` and `wrap --event` exit 6 on a reserved prefix; tender's internal call sites (sidecar lifecycle, exec's own `exec.*` events, rotation's `segment.opened`) are the only writers of reserved kinds, and the append layer itself performs no kind filtering. `hook.` is deliberately **unreserved**: the conventional namespace for external lifecycle-hook events (Claude Code, CI), published via `wrap --event hook.*` or direct `emit`. Payload-breaking change ⇒ new kind (`exec.result.v2`), not a `v` bump. |
| `namespace` / `session` | strings | yes | Shipped validation; kept as two fields. |
| `run_id` | UUIDv7 | yes | The supervised run (emitter's `TENDER_RUN_ID` or the sidecar's own). May name a prior generation after `--replace` — correct, not an error. |
| `gen` | u64 | no | Generation, when known. |
| `writer` | UUIDv7 | yes | Emitting **process** (sidecar: its run_id; CLI emitters mint one at startup). |
| `seq` | u64 from 1, contiguous per writer | yes | Gap detection + deterministic merge tiebreak. Replaces the daemon-requiring global `monotonic_seq`. |
| `source` | validated `Source` | yes | Semantic emitter: `tender.sidecar`, `tender.exec`, `claude.hook`, … `tender.*` reserved. |
| `block_id` | UUIDv7 | no | Command block (exec invocation) this event belongs to. ≈ OTel span_id. |
| `parent_id` | UUIDv7 | no | Immediate causal parent. Defaults from `TENDER_BLOCK_ID` (or `TENDER_PARENT_EVENT_ID`, §4). |
| `data` | object | no | Payload, ≤16 KiB serialized, else spill. |
| `data_ref` | `{path,bytes,sha256,media_type}` | no | Spill reference (§3.4). Present ⇒ `data` is a ≤4 KiB preview and `truncated:true`. |
| `truncated` | bool | no | Inline payload is a preview/truncation (shipped exec flag name). |

Lifecycle kind names reuse the shipped watch vocabulary verbatim
(`run.starting` … `run.dependency_failed`) plus new `pty.control_changed`,
`callback.finished`, `segment.opened`.

### 1.1 Worked examples

**(a) Lifecycle transition** — sidecar, at the transition, WAL-ordered
before `write_meta_atomic`:

```json
{"v":1,"id":"01981f2e-9a3b-7c1d-8e4f-0a1b2c3d4e5f","ts":"2026-07-06T03:14:15.926535Z","kind":"run.exited","namespace":"default","session":"build","run_id":"01981f2d-1111-7abc-9def-556677889900","gen":3,"writer":"01981f2d-1111-7abc-9def-556677889900","seq":7,"source":"tender.sidecar","data":{"status":"Exited","reason":"ExitedError","exit_code":3,"provenance":"direct"}}
```

**(b) Hook event via wrap** — `tender wrap --source claude.hook --event
hook.post_tool_use -- ~/.claude/hooks/post.sh` (data = wrap's shipped shape):

```json
{"v":1,"id":"01981f30-2222-7abc-8def-aabbccddeeff","ts":"2026-07-06T03:14:16.101833Z","kind":"hook.post_tool_use","namespace":"default","session":"agent","run_id":"01981f2d-1111-7abc-9def-556677889900","gen":3,"writer":"01981f30-2221-7abc-8def-001122334455","seq":1,"source":"claude.hook","parent_id":"01981f2f-3333-7abc-8def-667788990011","data":{"hook_stdin":{"tool_name":"Bash"},"hook_stdout":"","hook_stderr":"","hook_exit_code":0,"command":["/Users/rick/.claude/hooks/post.sh"],"truncated":false}}
```

**(c) Arbitrary emit** — `tender emit --kind build.finished --source
ci.local --data '{"ok":true,"artifacts":3}'`:

```json
{"v":1,"id":"01981f31-4444-7abc-8def-102030405060","ts":"2026-07-06T03:15:02.007412Z","kind":"build.finished","namespace":"default","session":"build","run_id":"01981f2d-1111-7abc-9def-556677889900","gen":3,"writer":"01981f31-4443-7abc-8def-605040302010","seq":1,"source":"ci.local","data":{"ok":true,"artifacts":3}}
```

**(d) Exec result with oversize spill** (slice 3 kind) — 1 MiB stdout,
blob keyed by content hash:

```json
{"v":1,"id":"01981f32-5555-7abc-8def-abcdefabcdef","ts":"2026-07-06T03:16:40.550021Z","kind":"exec.result","namespace":"default","session":"duck","run_id":"01981f2d-1111-7abc-9def-556677889900","writer":"01981f32-5554-7abc-8def-fedcbafedcba","seq":2,"source":"tender.exec","block_id":"01981f32-5550-7abc-8def-111122223333","data":{"exit_code":0,"cwd_after":"/data","timed_out":false,"stderr":"","stdout_preview":"┌─────────┐…","truncated":true},"data_ref":{"path":"events/blobs/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","bytes":1048576,"sha256":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","media_type":"application/json"}}
```

## 2. Identity & causality

Scope chain: `(namespace, session)` → `run_id` → `block_id` → `id`, plus
`parent_id` edges. Consumer tree-rebuild is three foreign keys, one DuckDB
pass; OTel export is mechanical (`run_id`→trace_id, `block_id`→span_id,
`parent_id`→parent_span_id).

Env propagation: the five shipped `TENDER_*` vars unchanged; **new**
`TENDER_BLOCK_ID` set by `exec`/`wrap` per invocation, and
`TENDER_PARENT_EVENT_ID` set by `wrap` (the id of the event it will write)
so hook-spawned `tender emit` calls chain to the hook event automatically.
Shipped surface (slice 3, 2026-07-08): exec exports `TENDER_BLOCK_ID`
into the payload for **PosixShell targets only** — PowerShell/PythonRepl
frames are deferred until a consumer demands it, DuckDB structurally
can't; wrap sets both vars **only on the dual-write path** (valid dotted
`--event` — the legacy annotation-only path sets no env chain, §6).
Malformed ambient values warn on stderr and are ignored — a polluted
environment never hard-fails a producer.

## 3. Storage & write path

### 3.1 Layout — per-session only

```
~/.tender/sessions/<ns>/<session>/
  meta.json          # unchanged — CURRENT STATE authority
  output.log         # unchanged — raw output + A-line projection
  events/
    <seg-uuidv7>.jsonl   # event segments — HISTORY authority
    blobs/<sha256>       # spilled payloads, content-addressed
    append.lock          # advisory lock file (POSIX only, §3.2)
```

No namespace-level file, canonical or derived: it would recouple retention,
reintroduce cross-session interleaving, and buy nothing — namespace views
are read-time merges (the same `session::list` walk watch does today).

### 3.2 Append protocol (all writers)

1. Pick the lexicographically greatest `events/*.jsonl` (UUIDv7 names sort
   by creation); if none, `create_new` a fresh one (race loser opens the
   winner's).
2. Open `OpenOptions::create(true).append(true)`.
3. POSIX only: take `flock(LOCK_EX)` on `events/append.lock` (a dedicated
   lock file, never the data file). Windows: **no lock** — see below.
4. Serialize the whole line, issue **one** `write_all`, release, close (CLI)
   or cache (sidecar).

The verified mechanics this rests on (adversarial verification against
POSIX text, man pages, MSDN, fsdevel — 2026-07-06):

- POSIX guarantees O_APPEND atomic offset-positioning, but **never**
  content non-interleaving on regular files at any size — the "PIPE_BUF
  applies to files" claim in older docs is retracted; PIPE_BUF is
  pipes/FIFOs only. Linux serializes whole `write()` calls via the inode
  lock in practice, but **macOS/APFS has no citable guarantee** — which is
  why POSIX writers take the two-syscall advisory flock: it converts
  folklore into contract among tender's cooperating writers, costs nothing
  at tender's event rates, and never blocks readers (readers don't take it).
- Windows `OpenOptions::append` deliberately strips `FILE_WRITE_DATA`,
  giving a **documented** per-WriteFile atomic-append contract across
  processes on NTFS (this is Rust stdlib policy, not folklore). No lock
  needed or taken; `LockFileEx` is mandatory and would block tailers.
- NFS/SMB session roots are unsupported (O_APPEND is a documented race on
  NFS) — same status as today's output.log.

Defense in depth regardless: JSONL is self-synchronizing (torn/foreign
fragments fail parse; readers resync at next `\n` and count skips) and
per-writer contiguous `seq` turns silent loss into a detected gap.

### 3.3 Rotation — no renames, ever

Sidecar-only, at ≥64 MiB: `create_new` the next segment, write
`segment.opened`, switch handles. Segment names are permanent identities —
no rename races, no `tail -F` semantics, no inode assumptions in cursors,
no NTFS sharing-violation hazards. CLI emitters never rotate; they pick the
newest segment at open time.

### 3.4 Oversize spill — content-addressed

`data` >16 KiB: write full `data` JSON to `events/blobs/<sha256>` (temp +
rename, same dir), append event with ≤4 KiB preview + `truncated:true` +
`data_ref`. sha256 keying dedupes identical payloads within a session and
is already the CAS key if that backlog item ever ships. Blob-write failure
degrades to inline truncation (the shipped wrap three-tier pattern) — never
a drop.

### 3.5 fsync

Default none (page-cache durability: survives emitter crash; power loss
risks only the writeback window). `--durable` on emit and **all sidecar
terminal transitions** use `fdatasync` (`F_FULLFSYNC` on macOS).

### 3.6 WAL ordering (graft from the single-writer design)

The sidecar appends the lifecycle event **before** `write_meta_atomic`, and
fdatasyncs the segment before any *terminal* meta write. This is an ordering
guarantee against the crash window: a sidecar that dies between the two writes
leaves the event, not terminal meta alone. It is not an IO-failure guarantee:
if the append itself fails, supervision still writes terminal meta (current
state remains authoritative), records a meta warning, and salvages the
fully-addressed terminal event to `~/.tender/lost+found/events.jsonl`.
Reconciliation gains `Evidence::EventLogTerminal`: before the CLI infers
`run.sidecar_lost`, it reads the event-log tail; if the sidecar's own
terminal event exists, meta is healed from it instead of inferred.

### 3.7 Retention

`prune` deletes the session dir; events and blobs ride along — zero new
machinery. Additions: `--events-keep-segments N` (the only operation that
can invalidate a live cursor), sweep of `lost+found` (§7) and any
carry-forward orphans, and adopting the known-orphaned `callbacks/*.json`.

## 4. Ordering & timestamps — the honest contract

Guaranteed: per-writer total order (`seq`, gaps detectable); declared causal
order (`parent_id`/`block_id`); deterministic cross-writer merge on
`(ts, writer, seq)`; occurrence-time stamps (inferred transitions say so:
`data.provenance:"inferred"`).

**One provenance vocabulary, two views.** `data.provenance` in lifecycle
events reuses the shipped `TransitionProvenance` vocabulary verbatim
(direct/inferred + evidence kinds, src/model/provenance.rs — the completed
provenance-on-lifecycle-transitions work). meta.json's
`transition_provenance` remains the *current-state* mirror (last transition
only); the event log is the provenance *history*. No second vocabulary is
ever introduced.

Not guaranteed: global cross-writer/cross-session/cross-host total order;
wall-clock monotonicity (clock steps — `seq` is the per-writer truth). No
PIPE_BUF claims, no HLC (single-host writers share one clock).

## 5. Read surface

### 5.1 `tender events`

```
tender events [--namespace ns] [--session ns/name]… [--kind prefix]…
              [--source prefix]… [--follow] [--from-now | --from-cursor <c>
              | --since <rfc3339>] [--last N] [--cursors] [--include-logs]
              [--strict]
```

Replay = read all segments of matching sessions in name order, merge by
`(ts, writer, seq)`, NDJSON to stdout. Follow = poll at 100 ms (the shipped
constant; optional `notify` wake-up hint later — hint only, poll remains
the protocol). `--last N` = tail-N warm start (completer's query).
`--include-logs` projects output.log O/E lines in at read time as derived
events (`"derived":true`, no stored identity) — the single merged stream
the egui terminal needs.

### 5.2 Cursors — Kubernetes semantics on files

Opaque base64 token over `{file, offset}` streams. Never renamed segments ⇒
cursors are valid for the life of their files. `--cursors` interleaves
read-time `cursor.bookmark` records (every 100 events / 5 s idle). A gone
segment ⇒ **exit 44** + structured stderr
`{"error":"cursor_gone","gone":[…],"recover":"…"}` — defined staleness,
defined recovery, never a silent restart from zero.

### 5.3 `tender watch`

Output shape frozen (f64 ts, kind/name split, event names). Internally
re-backed by the event log when `events/` exists (consumers silently gain
true timestamps, un-collapsed transitions, real sources); falls back to
meta-diff synthesis for old sessions. Documented as the compat surface.

## 6. Write surface

```
tender emit --kind <kind> [--data <json> | --data-file <p> | --data-stdin]
            [--source <source>] [--session <ns>/<name>] [--parent <uuid>]
            [--durable] [--best-effort]
```

Exit codes (granular, agents branch on integers): `0` ok, `2` usage,
`3` no session context and none given, `5` session not found, `6` invalid
kind/source (reserved prefix). `--best-effort`: all failures ⇒ exit 0
(hooks must never fail their host tool). `--source` defaults to
`user.emit`; a hook wanting attribution passes it explicitly
(`--source claude.hook`). The ambient chain is live since slice 3
(shipped 2026-07-08): `block_id` ← `TENDER_BLOCK_ID`; `parent_id` ←
explicit `--parent` > `TENDER_PARENT_EVENT_ID` > `TENDER_BLOCK_ID`. An
explicit `--parent` that fails to parse stays exit 2; malformed *env*
values warn and are ignored.

`wrap` and `exec` are sugar over the same append (shipped 2026-07-08,
slice 3). exec emits `exec.started`/`exec.result` from its internal call
site — permitted precisely because that kind value is tender-stamped,
not user-supplied; its A-line carries additive `event_id`/`block_id`.
wrap's user-supplied `--event` shipped with **one intentional
compatibility carve-out** (slice 3's only deviation — everything else
shipped as planned for emit, exec, and the sidecar facts):

- A **dotted protocol-valid kind dual-writes**: stored event + A-line
  linked by `event_id`/`block_id`, child env chain set (§2).
- A **reserved kind exits 6 before any side effect** — the child never
  spawns, nothing is written.
- A **legacy dotless name** (not a valid kind) keeps the exact
  pre-slice-3 behavior: annotation-only — no stored event, no child env
  chain — plus a stderr notice that fires on every such invocation.

"Reserved *kinds*" is the precise term (not "all invalid kinds"): a
grammar-invalid name is not a kind and takes the legacy path — it can
never produce a stored event, so the reserved namespace is not at risk.
Consumer caveat: wrap pre-mints its event id into the child's
`TENDER_PARENT_EVENT_ID` before spawn, so if wrap's best-effort append
later fails, child-emitted events reference a parent that was never
written and `parent_id → id` joins drop those edges (the A-line still
records `block_id`, without `event_id`).

## 7. Orphan emitters — lost+found

Emit from a process whose session dir was pruned/replaced mid-run: the
fully-addressed event is appended to `~/.tender/lost+found/events.jsonl`
and emit exits 0 (data preserved, never a resurrected session dir). Swept
by `prune`. After `--replace`, old-generation emits land in the
carried-forward log with the old `run_id` — attributable history.

## 8. Remote & Windows

`events` joins `REMOTE_COMMANDS` (read-only NDJSON over `ssh -T`, the
shipped watch/log shape) in slice 5 — until then `events`, like `emit`, is
local-only and `--host` rejects it (the CLI's local-only help text follows
in the same slice). Cursors are host-scoped; multi-host consumers
merge client-side by `ts`, honestly best-effort. `emit`/`wrap`/`exec`
remain local-to-the-session's-host (children run that host's binary).
Windows: the documented append contract (§3.2), `CREATE_NEW` races, no
renames, no locks; PowerShell exec side-channel untouched (the CLI writes
`exec.result` after reading it).

## 9. Failure modes (summary)

Torn line → parse-skip + counted (`--strict` ⇒ exit 65) + seq gap. Sidecar
crash mid-dual-write → WAL order bounds the loss to zero for terminal
transitions (§3.6), one non-terminal record otherwise, healed by the same
reconciliation that heals meta. Rotation-vs-reader race → structurally
impossible (no renames). Clock steps → `seq` is truth. Backpressure → none
needed; the disk is the buffer; a slow consumer costs writers nothing.
Fallen-behind follower → cursor-gone 44, defined recovery.

## 10. Superseded-schema reconciliation

| Old (doc) | Old field/behavior | Here |
|---|---|---|
| emit plan | `id` | `id` (UUIDv7, unchanged meaning) |
| emit plan | ISO `ts` | `ts` (format pinned to µs/Z) |
| emit plan | `tags[]` | dropped — use `kind` prefixes + `data` |
| emit plan | reject >256 KiB | spill (§3.4) |
| emit plan | `TENDER_BLOCK_ID` → `parent_block_id` | → `parent_id` |
| emit plan | `TENDER_SOCKET` daemon ingest | deleted — direct append |
| persistence spec | `event_id` | `id` |
| persistence spec | `monotonic_seq` (global) | `seq` (per-writer) + merge rule |
| persistence spec | `kind` closed enum | open vocabulary |
| persistence spec | `block_id` "belongs to" | `block_id` (same) + `parent_id` (causal) |
| persistence spec | `payload_blob` | `data_ref` (sha256-keyed) |
| persistence spec | PIPE_BUF atomicity claim | retracted (§3.2) |
| Hermes bridge | UUIDv4 envelope, `--json` CLI | deleted; bridge becomes a consumer (`events --follow --kind hook.` + `emit`) |
| docs wave | ULID identity | UUIDv7 |
| block-runtime spec | `tender event emit`, `tender watch --namespace --json`, `parent_block_id`, `tender block get` | `tender emit`, `tender events`, `parent_id`, `tender events --session` (naming settled here; that doc's reconciliation pass defers to this table) |

## 11. Implementation

Sliced in
[`2026-07-07-event-emit-primitive.md`](../completed/2026-07-07-event-emit-primitive.md)
(slice 1 = envelope + append + sidecar WAL lifecycle events + emit + replay —
**shipped 2026-07-07, PR #4**). Slice 2 (follow, cursors, re-backed watch —
**shipped 2026-07-07, PR #7**) landed via
[`2026-07-07-event-follow-cursors.md`](../completed/2026-07-07-event-follow-cursors.md):
`--follow`/`--from-now`/`--since`/`--last`, cursor tokens with exact resume
and cursor-gone exit 44, `--cursors` bookmarks, `--include-logs`
projection, and watch re-backed by the event log with its output shape
frozen. Slice 3 (exec/wrap integration: `exec.*` kinds,
`TENDER_BLOCK_ID`/`TENDER_PARENT_EVENT_ID` ambient causality,
`callback.finished`, `pty.control_changed` — **shipped 2026-07-08,
PR #10, main@7c91532**) landed via
[`2026-07-08-event-exec-wrap-integration.md`](../completed/2026-07-08-event-exec-wrap-integration.md):
shipped as planned for emit, exec, and the sidecar facts, with the wrap
`--event` compatibility carve-out (§6) as the one intentional
adjustment. Blocks/sugar beyond that, log
lifecycle (slice 4), and reach follow (slice 5) remain unscheduled. The
`--replace` events carry-forward is deliberately deferred and gated on a
demonstrated consumer need (the judges split on it; shipped replace
semantics stand until the block terminal demands cross-generation history —
implement then as per-segment moves, not a dir rename).
