---
id: event-log-analytics
depends_on:
  - event-emit-primitive
links:
  - ../specs/tender-as-block-runtime.md
  - content-addressable-storage.md
---

# Event-Log Analytics via DuckDB (JSONL + Parquet)

> **Schema superseded 2026-07-06 by [event-protocol.md](../specs/event-protocol.md).**
> The schema table and example queries below use `type`, `payload`,
> `payload_blob`, `parent_block_id`, `monotonic_seq`, and ULID `event_id` —
> none of which exist in the adopted protocol. The v1 envelope is `kind`,
> `data`, `data_ref`, `parent_id`, `writer`, `seq`, UUIDv7 `id`
> (event-protocol §1). Reframe v1 of this plan as: DuckDB over
> `~/.tender/sessions/*/*/events/*.jsonl` plus a compatibility SQL view —
> not a new analytics schema. The staged v1→v2→v3 rollout and the
> DuckDB-not-bespoke-code stance survive; the daemon/index references do not.

Expose Tender's event stream as a queryable analytical store via DuckDB. Write JSONL for the hot path, periodically roll cold segments to Parquet, present both transparently as a single SQL view. Zero bespoke analytics code in Tender — lean entirely on DuckDB.

## Why

Once events are first-class (see [event-emit-primitive](../completed/2026-07-07-event-emit-primitive.md)), the question becomes: how does anyone *analyze* them? Real questions worth answering with one SQL query:

- Top 10 longest-running blocks today
- Tool-call breakdown for one Claude Code session (most-used tools, total time per tool)
- Failure rate by host over the last 7 days
- Causal-chain walk: this failed block + every ancestor that led to it
- Time spent per project (cwd-grouped) per week
- "Which hosts ran a command that touched `/etc` last month?"

Writing each of these as bespoke Rust code is enormous; writing them as SQL against the event log is one query each. DuckDB is the right tool for the job: in-process, columnar-format-aware, native JSON support, no server.

## Goal

Two surfaces:

```bash
tender query "SELECT host, COUNT(*) FROM events WHERE ts > NOW() - INTERVAL 1 DAY GROUP BY host"
tender query --file analyses/failure-rate.sql
```

```bash
# OR — for serious work, drop into a DuckDB shell with the view pre-configured
tender query --shell
```

Under the hood: `tender query` invokes DuckDB (vendored or via runtime dep), pre-registers a view that unions all event-log segments for the requested namespace(s), and runs the user's SQL.

## First Slice Goal

The minimum useful thing: **a `tender query` subcommand that runs DuckDB against the existing JSONL event log**. No Parquet, no rollover, no manifest — just JSONL + DuckDB.

```bash
tender query "SELECT COUNT(*) FROM events"
tender query "SELECT type, COUNT(*) FROM events GROUP BY type ORDER BY 2 DESC"
```

This is shippable in days. Adds maybe 200 lines of Rust (subcommand + shelling out to `duckdb` CLI or vendored libduckdb).

## Three-stage rollout

| Stage | What ships | Code estimate |
|-------|-----------|---------------|
| **v1** | `tender query <SQL>` against JSONL | ~200 lines |
| **v2** | Periodic Parquet rollover; transparent union view | +100 lines + `parquet` crate or DuckDB-shelled conversion |
| **v3** | Object-storage backend for fleet mode (DuckDB `httpfs` reads S3-hosted Parquet) | +200 lines |

Ship v1 first. v2 lands when measured JSONL scan time exceeds budget. v3 when fleet mode is real.

## Layout

### v1 layout (JSONL only)

```
namespaces/<name>/
└── events.jsonl
```

`tender query` registers:

```sql
CREATE VIEW events AS
  SELECT * FROM read_json('events.jsonl', format='newline_delimited');
```

### v2 layout (hot JSONL + cold Parquet)

```
namespaces/<name>/events/
├── current.jsonl                 ← active writes (hot)
├── 2026-05-22.parquet            ← rolled-over, compressed (cold)
├── 2026-05-23.parquet
├── 2026-05-24.parquet
└── manifest.json                 ← segment list + per-segment schema_version
```

View transparently unions:

```sql
CREATE VIEW events AS
  SELECT * FROM read_parquet('events/*.parquet')
  UNION ALL
  SELECT * FROM read_json('events/current.jsonl', format='newline_delimited');
```

Rollover trigger: daily at midnight, OR when `current.jsonl > 100 MB`, whichever first. Conversion: shell out to DuckDB (`COPY current.jsonl TO 'YYYY-MM-DD.parquet' (FORMAT parquet, COMPRESSION zstd)`) or use the `parquet` Rust crate directly.

### v3 layout (fleet mode)

Same shape, but the directory roots live in object storage (S3-compatible). DuckDB's `httpfs` extension reads remote Parquet transparently. Hot `current.jsonl` stays local until rollover, then uploads.

## Schema design

Two decisions matter for analytics-friendliness:

### 1. Payload as a JSON column, not flattened

```
schema_version   INTEGER
event_id         VARCHAR             -- ULID
ts               TIMESTAMP_NS        -- not ISO-string; native time type
monotonic_seq    BIGINT
namespace        VARCHAR
kind             VARCHAR             -- 'lifecycle' | 'emitted' | 'annotation' | 'artifact-ref'
type             VARCHAR             -- e.g. 'tool-start', 'lifecycle:exited'
block_id         VARCHAR
parent_block_id  VARCHAR
actor            VARCHAR
host             VARCHAR
payload          JSON                -- varies by type; DuckDB queries with -> and ->>
payload_blob     VARCHAR             -- sha256 ref if payload exceeded inline cap
```

Different event types have different payloads. Flattening every variant into a wide table would produce a ~100-column mostly-NULL table. JSON column + DuckDB's JSON functions is faster, smaller, and cleaner.

### 2. `TIMESTAMP_NS` for time

Native DuckDB time type. Partition pruning, range queries, and time-series functions all work natively. Avoid ISO-8601 strings — the parsing tax compounds at scale.

## Example queries that just work

```sql
-- Top 10 longest-running blocks today
SELECT block_id,
       (completed_at - started_at) / 1e6 AS dur_sec,
       payload->>'cmd' AS cmd
FROM events
WHERE type = 'lifecycle:exited'
  AND DATE_TRUNC('day', ts) = CURRENT_DATE
ORDER BY dur_sec DESC LIMIT 10;

-- Claude Code tool-call breakdown for one session
SELECT
  payload->>'tool'                              AS tool,
  COUNT(*)                                      AS calls,
  SUM((payload->>'duration_ms')::INT)           AS total_ms,
  AVG((payload->>'duration_ms')::INT)           AS avg_ms
FROM events
WHERE type = 'tool-end'
  AND parent_block_id = '01HX…'
GROUP BY tool ORDER BY total_ms DESC;

-- Failure rate by host over the last 7 days
SELECT host,
       COUNT(*) FILTER (WHERE (payload->>'exit_code')::INT != 0) AS failures,
       COUNT(*) AS total,
       ROUND(100.0 * COUNT(*) FILTER (WHERE (payload->>'exit_code')::INT != 0)
                   / COUNT(*), 2) AS failure_pct
FROM events
WHERE type = 'lifecycle:exited'
  AND ts > NOW() - INTERVAL 7 DAY
GROUP BY host ORDER BY failure_pct DESC;

-- Walk causal chain from one block to its root
WITH RECURSIVE chain AS (
  SELECT * FROM events WHERE block_id = '01HX…' AND type = 'lifecycle:created'
  UNION ALL
  SELECT e.* FROM events e
  JOIN chain c ON e.block_id = c.parent_block_id
  WHERE e.type = 'lifecycle:created'
)
SELECT block_id, parent_block_id, payload->>'cmd' AS cmd FROM chain;
```

## Performance budgets

| Operation | Target |
|---|---|
| Append one event | < 100 µs (existing JSONL write path) |
| `tender query` cold-start (load DuckDB + register view) | < 100 ms |
| Scan 1 GB JSONL | < 1 s (DuckDB streaming) |
| Scan 1 GB Parquet (column-pruned) | < 50 ms |
| Time-range query over 1 year of Parquet | < 200 ms (partition pruning) |
| Storage compression (Parquet+zstd vs raw JSONL) | 3–10× smaller |

Hot writes are unchanged from the JSONL design. Analytics is read-only and offline-ish (you run a query when you want one), so even multi-second queries are acceptable for the largest tasks.

## Why DuckDB and not a transactional DB

Transactional databases (SQLite, Postgres) are good at point lookups and concurrent writes. They are not good at "scan 100M rows grouped by host" — that's what columnar formats exist for. Tender's storage layering separates concerns:

| Concern | Tool | Why |
|---|---|---|
| Live state ("which blocks are running?") | In-memory index (built from event log on startup) | Sub-microsecond lookups; no separate file format |
| Append-only audit / log of truth | JSONL files (rolled to Parquet) | Atomic single-line appends; columnar cold storage |
| Analytics ("aggregate over N events") | DuckDB over JSONL + Parquet | Columnar, vectorized, full SQL |

Each tool plays its strength; no transactional DB in the design.

## Scope

- New `tender query <SQL>` and `tender query --file <path>` and `tender query --shell` subcommands
- Auto-register `events` view spanning all segments of the current/selected namespace(s)
- Optional v2: `tender compact` (or background task) rolls hot JSONL into dated Parquet files; updates `manifest.json`
- Documented schema with `schema_version` field
- Example SQL snippets in `docs/analytics-recipes.md`

## Non-goals

- **No web UI / dashboarding.** That's a downstream consumer. Tender exposes the SQL surface; UIs build on top.
- **No streaming analytics in Tender.** If users want push-based analytics, they subscribe to `tender watch --json` and run their own consumer; Tender's analytics are pull-mode SQL.
- **No data warehouse integration in v1.** Export to Snowflake / BigQuery / etc. is a downstream consumer concern; we expose Parquet, the rest is plumbing.
- **No materialized views beyond what DuckDB itself does.** If a user wants a pre-aggregated rollup, they write a SQL CTE or store results themselves.
- **No replacing the in-memory state index.** The daemon's in-memory `HashMap<BlockId, BlockState>` (built from the event log at startup) serves live state. DuckDB is read-only over the same log for analytical queries.
- **No federation across hosts in v1/v2.** Fleet mode (v3) reads multiple hosts' Parquet from object storage; cross-host live queries deferred.

## Acceptance criteria

### v1

- `tender query "SELECT COUNT(*) FROM events"` returns the correct count from the active JSONL log
- `tender query --file path/to/query.sql` runs SQL from a file
- `tender query --namespace foo` scopes the view to one namespace; default = all namespaces
- Schema documented (event columns + payload JSON shape per `type`)
- DuckDB version pinned; reproducible via `tender query --version`
- Works cross-platform (macOS, Linux, Windows)
- At least 5 example queries documented in `docs/analytics-recipes.md`

### v2

- `tender compact --namespace foo --before <ts>` rolls JSONL prefix → dated Parquet, updates manifest
- Automated daily rollover via existing Tender daemon (configurable interval)
- View auto-spans Parquet + hot JSONL; queries are oblivious to rollover
- Parquet files use zstd compression, partitioned by date
- `tender prune --events --before <ts>` removes Parquet files older than threshold
- Performance: 1-year of typical events queryable in < 1 s for time-bounded queries

### v3

- Manifest stored in object storage; segments uploaded on rollover
- DuckDB `httpfs` extension reads from S3-compatible storage transparently
- Fleet-mode `tender query --fleet` aggregates across configured remote hosts' manifests
- Caching of recently-read Parquet to local FS to avoid re-download

## Open questions

1. **Vendor DuckDB or require it as a runtime dep?** Vendoring (libduckdb static link) gives single-binary distribution but bloats Tender's binary by ~25 MB. Runtime dep keeps Tender tiny but adds an install step. Recommend: **runtime dep for v1** (find DuckDB on PATH; install instructions in docs), revisit vendoring if Tender grows other DuckDB-dependent features.
2. **DuckDB version pinning.** Tender should pin a specific minor version and fail clearly if the user's DuckDB is older. Recommend support latest minor at time of release; document the supported range.
3. **Schema migration when Parquet segments differ in `schema_version`.** Per-segment schema_version in the manifest; the view either casts forward or rejects mixed versions until a migration runs. Recommend explicit `tender migrate-events` subcommand that rewrites old-version Parquets in place.
4. **Compaction granularity.** Daily Parquet files are clean but produce many small files for active users. Could compact daily → weekly → monthly over time. Recommend daily for v2; revisit compaction tiers when measuring matters.
5. **Hot JSONL durability.** Same as the event-emit-primitive question — fsync each event vs batched. Inherits whatever that plan decides.
6. **Multi-namespace queries.** `tender query --namespace a,b,c` unions multiple namespaces' segments. Recommend yes from v1 — it's a one-line view change.
7. **JSON column query ergonomics.** DuckDB's `->`, `->>`, `json_extract_string` work but are verbose for nested paths. Consider shipping helper SQL functions or views that pre-extract common payload fields per event type. Recommend defer — DuckDB's native JSON is good enough at v1.
8. **Replay-to-rebuild.** The in-memory state index is rebuilt from the event log at every daemon startup. Optionally a snapshot file can be kept for faster restart; the log is always the source of truth. Out of scope for this plan but worth noting the analytics path doesn't preclude it.

## How this composes with adjacent plans

- [event-emit-primitive](../completed/2026-07-07-event-emit-primitive.md) — produces the JSONL the analytics layer consumes. Schema agreed there is what we query here.
- [content-addressable-storage](content-addressable-storage.md) — blob refs in events are queryable too; analytics can join against blob metadata.
- [boundary-metadata](boundary-metadata.md) — `boundary_kind` / `boundary_label` columns on events make "what ran in containers vs hosts" a trivial GROUP BY.
- [provenance-on-lifecycle-transitions](../completed/2026-04-16-provenance-on-lifecycle-transitions.md) — `transition_provenance.kind` (direct vs inferred) becomes another column for analytics: "what % of lifecycle conclusions are inferred vs observed?"
- [egui-block-terminal](egui-block-terminal.md) — the GUI consumes the live event stream, but ad-hoc analytics inside the GUI ("show me a chart of tool durations") just runs DuckDB queries the same as CLI.

## Depends On

- `event-emit-primitive` — the JSONL events to be queried don't exist without it.
