# Event-log analytics recipes

`tender query` points the external [DuckDB](https://duckdb.org) CLI at the
on-disk JSONL event log and runs your SQL against an auto-registered `events`
view. It is the **analytical** surface (aggregate SQL, offline); `tender events`
remains the **streaming/replay** surface. Read-only — analytics never writes to
the log.

Requires the `duckdb` CLI on your `PATH` (the same external binary the `duckdb`
exec target drives — not a tender dependency). Developed against DuckDB 1.x;
check yours with `tender query --version`.

## CLI surface

```sh
tender query "SELECT COUNT(*) FROM events"      # inline SQL
tender query --file analyses/failure-rate.sql   # SQL from a file
tender query --namespace agents "<SQL>"         # scope the view (comma-separated; default = all)
tender query --shell                            # DuckDB REPL with `events` pre-registered
tender query --version                          # report the DuckDB version in use
```

A failed query propagates DuckDB's non-zero exit code, so `tender query` is safe
in scripts and CI.

## The `events` view

Every line of every `events/*.jsonl` segment in scope becomes one row. The
envelope fields (event-protocol.md §1) are projected as typed columns; the
per-`kind` payload stays JSON so you query it with `->`/`->>`:

| Column | Type | Notes |
|---|---|---|
| `v` | INT | envelope version (`1`) |
| `id` | VARCHAR | UUIDv7 event identity |
| `ts` | TIMESTAMP | auto-cast from RFC-3339 µs; compare/`BETWEEN`/`::DATE` directly |
| `kind` | VARCHAR | dotted routing id, e.g. `exec.result`, `run.exited`, `hook.post_tool_use` |
| `namespace`, `session` | VARCHAR | scope |
| `run_id` | VARCHAR | supervised run (UUIDv7) |
| `gen` | UBIGINT | generation, when known |
| `writer`, `seq` | VARCHAR, UBIGINT | emitting process + per-writer sequence |
| `source` | VARCHAR | semantic emitter (`tender.sidecar`, `tender.exec`, `claude.hook`, …) |
| `block_id` | VARCHAR | command block (≈ span id) |
| `parent_id` | VARCHAR | immediate causal parent (≈ parent span id) |
| `data` | JSON | payload — query with `data->>'field'` (text) or `data->'field'` (JSON) |
| `data_ref` | JSON | `{path,bytes,sha256,media_type}` spill ref when `data` was truncated |

One bad line never kills a query: a valid line whose field has an unexpected
value gets a NULL in that column (`TRY_CAST`), and an unparseable or torn line is
skipped entirely (`ignore_errors`) — it is neither an error nor a counted row.

## Recipes

### 1. Event volume by kind

```sql
SELECT kind, COUNT(*) AS n
FROM events
GROUP BY kind
ORDER BY n DESC;
```

### 2. exec failure rate by session over the last 7 days

`data->>'exit_code'` reads the payload as text; cast it to compare numerically.

```sql
SELECT session,
       COUNT(*) FILTER (WHERE (data->>'exit_code')::INT != 0) AS failures,
       COUNT(*)                                               AS total
FROM events
WHERE kind = 'exec.result'
  AND ts > now() - INTERVAL 7 DAY
GROUP BY session
ORDER BY failures DESC;
```

### 3. The 10 longest command blocks today

Pair `exec.started` with `exec.result` on the shared `block_id`, and use `ts`
arithmetic for the duration.

```sql
SELECT s.block_id,
       epoch_ms(r.ts) - epoch_ms(s.ts) AS dur_ms,
       s.data->>'command'              AS command
FROM events s
JOIN events r USING (block_id)
WHERE s.kind = 'exec.started'
  AND r.kind = 'exec.result'
  AND s.ts::DATE = current_date
ORDER BY dur_ms DESC
LIMIT 10;
```

### 4. Walk a causal chain up through its ancestors

Follow `parent_id` edges from one event to the root with a recursive CTE.

```sql
WITH RECURSIVE chain AS (
  SELECT * FROM events WHERE id = '<event-id>'
  UNION ALL
  SELECT e.* FROM events e JOIN chain c ON e.id = c.parent_id
)
SELECT id, parent_id, kind, data->>'command' AS command
FROM chain;
```

### 5. Agent tool-call breakdown for one session

Over Claude Code hook events (emitted via `wrap --event hook.*` / `emit`).

```sql
SELECT data->>'tool' AS tool,
       COUNT(*)       AS calls
FROM events
WHERE kind = 'hook.post_tool_use'
  AND session = '<name>'
GROUP BY tool
ORDER BY calls DESC;
```

### 6. Run outcomes by exit reason

Lifecycle events carry the outcome in `data->>'reason'` (`ExitedOk`,
`ExitedError`, `Killed`, …) on `run.exited`.

```sql
SELECT data->>'reason' AS reason,
       COUNT(*)         AS runs
FROM events
WHERE kind = 'run.exited'
GROUP BY reason
ORDER BY runs DESC;
```

## Scoping notes

- `--namespace` accepts a comma-separated list (`--namespace default,agents`);
  omit it to query every namespace.
- Group by *host* / *boundary* using the immutable `data.boundary` snapshot that
  [boundary-metadata](plans/completed/2026-07-10-boundary-metadata.md) stamps
  onto `run.starting` / `run.started`. Filter to those launch events and
  `GROUP BY data->'boundary'->'current'->>'kind'` (or `->>'label'`); to bucket
  *other* events by boundary, join them to their run's launch event on `run_id`
  rather than to current `meta.json` — the snapshot is the historical authority,
  `meta.json` is only current state. The optional `boundary_kind` /
  `boundary_label` convenience columns remain a deferred `tender query` nicety.
- Save a query you run often to a `.sql` file and run it with `--file`.
