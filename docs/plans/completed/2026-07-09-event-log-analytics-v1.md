---
id: event-log-analytics
depends_on: []
links:
  - ../specs/event-protocol.md
  - ../specs/tender-as-block-runtime.md
  - ../backlog/content-addressable-storage.md
---

# Event-Log Analytics v1 — DuckDB over the JSONL event log

> **Shipped 2026-07-09 via PR #24 at main@`dc956a2`.**
> Implemented as `tender query`: inline SQL, `--file`, `--namespace`,
> `--shell`, and `--version`, backed by the external `duckdb` CLI on `PATH`
> with no new crate dependency. Covered by 11 `cli_query` tests plus the
> `cli_remote` local-only guard for `--host query`; recipes landed in
> [analytics-recipes.md](../../analytics-recipes.md). Plan archival follows the
> implementation PR by design.

Turn the shipped structured event stream into a queryable analytical store by
pointing DuckDB at the on-disk JSONL. **Zero bespoke analytics code in tender —
lean entirely on DuckDB** (already a supported `duckdb` exec target, i.e. a
known external tool found on PATH — not a Tender crate dependency). This is the
least-speculative consumer of the event protocol and the first proof it is
useful beyond logging.

> **Shipped deviation:** the plan sketched `read_json_auto(...)` over a glob.
> The implementation uses Rust-side segment discovery plus a projected DuckDB
> view over `read_json(..., records=false, ignore_errors=true)`. That keeps
> `data`/`data_ref` as JSON so `data->>'exit_code'` remains queryable, casts
> envelope columns explicitly, gives empty scopes an empty typed view, and skips
> malformed/torn lines instead of aborting a query.

> **Rescoped 2026-07-09 to the shipped protocol.** The prior draft's schema
> (`type`, `payload`, `payload_blob`, `parent_block_id`, `monotonic_seq`, ULID
> `event_id`) and its daemon/`namespaces/<name>/events.jsonl` layout never
> shipped. The adopted envelope and paths (below) come from
> [event-protocol.md](../specs/event-protocol.md), the schema owner. The
> staged v1→v2→v3 ambition and the DuckDB-not-bespoke-code stance survive; v2/v3
> are deferred to a historical note at the end.

## Why

`event-emit-primitive` shipped (`tender emit` + `tender events`), and
`exec`/`wrap` now dual-write structured events. So the events exist on disk —
what is still missing is a way to *ask questions* of them. Real questions worth
one SQL query each:

- Count / rate of each event `kind` over a window
- exec failure rate by session (or by boundary, once `boundary-metadata` lands)
- The 10 longest-running command blocks today (pair `exec.started`/`exec.result`)
- Walk a causal chain: a failed block and every ancestor via `parent_id`
- Tool-call breakdown for one agent session (over `hook.*` events)

Writing each as bespoke Rust is enormous; as SQL over the log it is one query.
DuckDB is the right tool: in-process, columnar, native JSON, no server, and it
auto-casts the RFC-3339-µs `ts` to a real timestamp.

## CLI surface — decision required

Two shapes were on the table; **this plan picks `tender query` and records why.**

| Option | Shape | Verdict |
|---|---|---|
| **A — new verb** | `tender query "<SQL>"` | **Chosen.** Analytics is a distinct concern from the live stream. `tender events` is the *streaming/replay* surface (follow, cursors, frozen output shape); `tender query` is the *analytical* surface (aggregate SQL, offline). Keeping them separate avoids overloading `events` and keeps each shape frozen. |
| B — subflag | `tender events --sql "<SQL>"` | Rejected for v1. It reuses `events`' namespace-scoping plumbing but conflates a streaming projection with an analytical engine, and `events`' output shape is frozen. |

Reconsider B only if `query` ends up duplicating a lot of `events`' segment
discovery — in which case factor the shared discovery into a helper, not a flag.

## First slice (v1)

A `tender query` subcommand that runs DuckDB against the existing per-session
JSONL. No Parquet, no rollover, no manifest.

```bash
tender query "SELECT kind, COUNT(*) FROM events GROUP BY kind ORDER BY 2 DESC"
tender query --file analyses/failure-rate.sql
tender query --namespace default            # scope the view; default = all namespaces
tender query --shell                        # drop into a DuckDB shell with the view pre-registered
```

Under the hood: locate the event segments for the requested namespace(s),
register a `events` view unioning them, run the user's SQL, print the result.
~200 lines of Rust (subcommand + shelling to the `duckdb` CLI on PATH — the
same external binary the `duckdb` exec target already drives; no new crate dep).

## Layout (shipped, v1)

Events live inside each session dir — no namespace-level files, no daemon:

```
~/.tender/sessions/<namespace>/<session>/events/
├── <uuidv7>.jsonl        ← append-only segments (lexicographically ordered)
└── blobs/<sha256>        ← spilled oversize payloads (data_ref targets)
```

The view globs every segment for the selected scope and reads `data` as JSON:

```sql
CREATE VIEW events AS
  SELECT * FROM read_json_auto(
    '~/.tender/sessions/*/*/events/*.jsonl',   -- or .../<ns>/*/... when --namespace given
    format = 'newline_delimited',
    union_by_name = true
  );
```

## Envelope (the columns you query) — event-protocol.md §1

Top-level fields, stamped by tender (trusted tier); `data` is the per-`kind`
payload:

| Column | Type | Notes |
|---|---|---|
| `v` | INT (`1`) | envelope version |
| `id` | UUIDv7 | event identity / dedupe key |
| `ts` | TIMESTAMP | RFC-3339 µs `Z`; DuckDB auto-casts |
| `kind` | VARCHAR | dotted routing id, e.g. `exec.result`, `hook.post_tool_use` |
| `namespace`, `session` | VARCHAR | scope |
| `run_id` | UUIDv7 | supervised run |
| `gen` | UBIGINT | generation, when known |
| `writer`, `seq` | UUIDv7, UBIGINT | emitting process + contiguous per-writer sequence (gap detection / merge tiebreak) |
| `source` | VARCHAR | semantic emitter (`tender.sidecar`, `tender.exec`, `claude.hook`, …) |
| `block_id` | UUIDv7 | command block (≈ span_id) |
| `parent_id` | UUIDv7 | immediate causal parent (≈ parent_span_id) |
| `data` | JSON | payload; query with `->`/`->>` |
| `data_ref` | STRUCT | `{path,bytes,sha256,media_type}` spill ref; present ⇒ `data` is a preview + `truncated` |

`data` shape varies by `kind` (open vocabulary), so it stays a JSON column
rather than a ~100-column mostly-NULL wide table. Scope chain for tree rebuild:
`(namespace, session) → run_id → block_id → id`, plus `parent_id` edges — three
foreign keys, one pass; OTel export is mechanical.

## Example queries (v1 envelope, real fields)

```sql
-- Event volume by kind
SELECT kind, COUNT(*) AS n FROM events GROUP BY kind ORDER BY n DESC;

-- exec failure rate by session over the last 7 days
SELECT session,
       COUNT(*) FILTER (WHERE (data->>'exit_code')::INT != 0) AS failures,
       COUNT(*)                                                AS total
FROM events
WHERE kind = 'exec.result' AND ts > now() - INTERVAL 7 DAY
GROUP BY session ORDER BY failures DESC;

-- 10 longest command blocks today (pair started/result on block_id)
SELECT s.block_id,
       epoch_ms(r.ts) - epoch_ms(s.ts) AS dur_ms,
       s.data->>'command'              AS command
FROM events s
JOIN events r USING (block_id)
WHERE s.kind = 'exec.started' AND r.kind = 'exec.result'
  AND s.ts::DATE = current_date
ORDER BY dur_ms DESC LIMIT 10;

-- Walk a causal chain from one block up through its ancestors
WITH RECURSIVE chain AS (
  SELECT * FROM events WHERE id = '<event-id>'
  UNION ALL
  SELECT e.* FROM events e JOIN chain c ON e.id = c.parent_id
)
SELECT id, parent_id, kind, data->>'command' AS command FROM chain;

-- Agent tool-call breakdown for one session (Claude Code hooks via `wrap --event hook.*`)
SELECT data->>'tool'                       AS tool,
       COUNT(*)                            AS calls
FROM events
WHERE kind = 'hook.post_tool_use' AND session = '<name>'
GROUP BY tool ORDER BY calls DESC;
```

(Grouping by *host*/*boundary* becomes trivial once
[boundary-metadata](2026-07-10-boundary-metadata.md) adds those columns to the
lifecycle events.)

## Scope (v1)

- `tender query "<SQL>"`, `tender query --file <path>`, `tender query --shell`
- `--namespace <a[,b,…]>` scopes the `events` view; default = all namespaces
- Auto-register the `events` view over all segments in scope, `data` as JSON
- Pin the DuckDB version; surface it via `tender query --version`
- Cross-platform (macOS, Linux, Windows)
- ≥5 example recipes in `docs/analytics-recipes.md`

## Non-goals (v1)

- No web UI / dashboarding — a downstream consumer builds on the SQL surface.
- No streaming/push analytics — that is `tender events --follow` + a consumer.
- No Parquet / rollover / object storage (that is v2/v3, deferred below).
- No bespoke query language or materialized views — DuckDB SQL is the surface.
- No new write path — analytics is strictly read-only over the shipped log.

## Acceptance criteria (v1)

- `tender query "SELECT COUNT(*) FROM events"` returns the correct count from the on-disk JSONL
- `tender query --file <path>` runs SQL from a file; `--namespace foo` scopes the view (default = all)
- `data` is queryable as JSON (`data->>'exit_code'` etc.); `ts` compares as a timestamp
- DuckDB version pinned and reported; clear error if a required DuckDB is absent
- Works on macOS, Linux, Windows
- ≥5 documented recipes in `docs/analytics-recipes.md`

## Deferred (v2/v3 — historical, not this slice)

Kept as direction only; do **not** build until measured JSONL scan time hurts:

- **v2** — periodic Parquet rollover of cold segments (`COPY … TO … (FORMAT parquet, COMPRESSION zstd)`), a transparent `read_parquet ∪ read_json` view, and `prune --events --before <ts>`. No daemon: rollover is a `tender compact` verb or a prune-time step.
- **v3** — fleet mode: segment roots in S3-compatible object storage, DuckDB `httpfs` reading remote Parquet, `tender query --fleet` across configured hosts.

Open decisions that survive into v1: **vendor libduckdb vs require `duckdb` on
PATH** (recommend PATH for v1, matching the shipped duckdb exec target), and
**DuckDB version pinning** (support the latest minor at release; document the
range).

## How this composes

- [event-emit-primitive](../completed/2026-07-07-event-emit-primitive.md) — produces the JSONL this queries.
- [boundary-metadata](2026-07-10-boundary-metadata.md) — adds boundary columns, making "hosts vs containers" a GROUP BY.
- [content-addressable-storage](../backlog/content-addressable-storage.md) — `data_ref` blob metadata is queryable/joinable if that lands.
- Downstream GUI consumers can run ad-hoc charts through the same DuckDB view.
