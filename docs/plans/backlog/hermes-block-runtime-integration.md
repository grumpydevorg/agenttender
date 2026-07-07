---
id: hermes-block-runtime-integration
depends_on:
  - event-emit-primitive
  - skill-claude-code
links:
  - ../specs/tender-as-block-runtime.md
  - ../completed/2026-07-07-event-emit-primitive.md
  - ../backlog/skill-claude-code.md
---

# Hermes Block-Runtime Integration

> **Superseded 2026-07-06 — rewrite pending.** Everything below predates
> [event-protocol.md](../specs/event-protocol.md): `tender event emit`,
> `watch --json`, `parent_block_id`, the custom envelope, UUIDv4 ids, and
> the "running Tender daemon" requirement no longer exist. The rewrite is a
> thin consumer: hook scripts run
> `tender emit --kind hook.hermes.<event> --source hermes.hook --data-stdin --best-effort`
> inside a supervised session; the bridge/UI reads
> `tender events --follow --kind hook.hermes.` (protocol slice 2). The
> namespace bug (emitting to `hermes-${SESSION_ID}` while every consumer
> reads `hermes`) dies with the rewrite. Retained value: the Hermes-side
> hook inventory and consent mechanism, pending verification against Hermes
> docs.

Make Hermes a first-class event producer for Tender's block-runtime stream.
No changes to either codebase — config + a thin shell bridge only.

## Verdict: valid pattern. Hermes hook payload shape already carries every
## field Tender's block event schema needs (tool name, args, result,
## duration_ms, session_id, cwd, parent_session_id). The bridge is a shell
## script under `hooks:` config, not an SDK integration.

---

## Problem

Hermes is an AI agent runtime with 16 lifecycle hooks (`pre_tool_call`,
`post_tool_call`, `on_session_start`, `post_llm_call`, ...). Tender's
block-runtime positioning says: any supervised process can emit structured
events into Tender's universal stream via `tender event emit`.

The gap: nobody has written the bridge. Hermes users know *that* hooks exist
and *that* Tender records events, but there is no canonical recipe for
connecting them.

## Fit Assessment

| What Hermes provides | What Tender consumes |
|---|---|
| `post_tool_call` with `tool_name, args, result, duration_ms` | block record with command spec + I/O + timing |
| `session_id`, `parent_session_id`, `task_id` | `block_id`, `parent_block_id`, causality |
| `post_llm_call` with model, token estimates | lifecycle: LLM usage events |
| `on_session_start` / `on_session_finalize` | session boundary events |
| `pre_tool_call` (blocking — veto, injection) | Tender records the *attempted* tool, then the *blocked* outcome |
| `transform_terminal_output` | captured stdout/stderr already lives in Tender's sidecar |

All fields map without transformation. No custom protocol needed.

## Architecture — Three Layers

```
Layer 5: Presentation   (NOT Tender, NOT Hermes)
  ┌────────────────────────────────────────────────┐
  │  tender-blocks-egui (or dashboard, audit log) │
  │  subscribes to tender watch --namespace hermes│
  └──────────────────────┬───────────────────────┘
                         │ NDJSON event stream
                         ▼
Layer 3: Block Runtime  (Tender owns)
  ┌────────────────────────────────────────────────┐
  │  tender watch --namespace hermes --json        │
  │  tender start --namespace hermes -- hermes     │
  │  tender event emit (ingests from bridge)        │
  └──────────────────────┬───────────────────────┘
                         │ tenv vars / stdin pipe
                         ▼
Layer 1: Bridge         (shell script — yours)
  ┌────────────────────────────────────────────────┐
  │  tender-emit.sh — translates Hermes hook JSON    │
  │  into Tender event schema and calls             │
  │  tender event emit                               │
  └──────────────────────┬───────────────────────┘
                         │ subprocess call via hooks:
  ┌──────────────────────┴───────────────────────┐
Layer 0: Agent          (Hermes owns)
  │  hermes agent/shell_hooks.py → invoke_hook()│
  │  reads hooks: from ~/.hermes/config.yaml      │
  └──────────────────────────────────────────────┘
```

## Required: Tender Side

1. `tender emit` shipped 2026-07-07 (completed: `2026-07-07-event-emit-primitive.md`)
2. A running Tender daemon on the local host or `--host` path

## Required: Hermes Side

Hermes `v0.14.0+` already has `hooks:` support in `~/.hermes/config.yaml`.

## Implementation: Step by Step

### Step 1: Write the bridge script

Create `~/.local/bin/tender-emit.sh` (chmod +x):

```bash
#!/usr/bin/env bash
set -euo pipefail

# tender-emit.sh
# Reads Hermes shell-hook JSON from stdin, translates to Tender event
# schema, and publishes via `tender event emit`.
#
# Called from Hermes hooks: on_session_start, post_tool_call,
# on_session_end, on_session_finalize, etc.

payload=$(cat)

# Extract known Hermes fields that Tender reuses
HERMES_EVENT=$(echo "$payload" | jq -r '.hook_event_name // empty')
SESSION_ID=$(echo "$payload" | jq -r '.session_id // empty')
TOOL_NAME=$(echo "$payload" | jq -r '.tool_name // empty')
DURATION_MS=$(echo "$payload" | jq -r '.extra.duration_ms // 0')
CWD=$(echo "$payload" | jq -r '.cwd // .extra.cwd // .cwd // "null"')

# Translate Hermes hook name to Tender event type
case "$HERMES_EVENT" in
    on_session_start)      TENDER_TYPE="block.start" ;;
    post_tool_call)        TENDER_TYPE="block.tool" ;;
    on_session_end)         TENDER_TYPE="block.end" ;;
    on_session_finalize)    TENDER_TYPE="block.finalize" ;;
    *)                      TENDER_TYPE="agent.event" ;;
esac

# Build the Tender event envelope.
# Schema version tracked for future migration.
cat <<EVENT | tender event emit --namespace "hermes-${SESSION_ID}" --json -
{
  "schema_version": 1,
  "type": "${TENDER_TYPE}",
  "block_id": "$(uuidgen | tr A-Z a-z 2>/dev/null || python3 -c 'import uuid; print(uuid.uuid4())')",
  "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "provenance": {
    "origin": "Agent",
    "actor": "Hermes",
    "actor_version": "${HERMES_VERSION:-unknown}",
    "host": "$(hostname)",
    "session_id": "${SESSION_ID:-null}"
  },
  "spec": {
    "argv": ["${TOOL_NAME}"],
    "cwd": ${CWD:-"null"}
  },
  "lifecycle": {
    "state": "done",
    "exit_code": 0
  },
  "metrics": {
    "duration_ms": ${DURATION_MS}
  },
  "payload": $(echo "$payload" | jq -c 'del(.hook_event_name, .session_id, .cwd)')
}
EVENT
```

**Notes:**
- `uuidgen` works on macOS. Fallback to Python `uuid.uuid4()`.
- The inner `payload` field carries the *original* Hermes hook data so
  downstream consumers can access tool-specific fields not in Tender's schema.
- `jq` is required. On Windows, use `choco install jq` or ship a small Go
  bridge instead of bash.

### Step 2: Register hooks in Hermes config

Add to `~/.hermes/config.yaml` (or `HERMES_CONFIG` path):

```yaml
hooks:
  on_session_start:
    - command: ~/.local/bin/tender-emit.sh
      timeout: 10

  post_tool_call:
    - command: ~/.local/bin/tender-emit.sh
      timeout: 10
      # Optional: filter to expensive/dangerous tools only
      matcher: "terminal|browser_|execute_code|patch|write_file"

  on_session_end:
    - command: ~/.local/bin/tender-emit.sh
      timeout: 10

  on_session_finalize:
    - command: ~/.local/bin/tender-emit.sh
      timeout: 10
```

**First-run consent:** Hermes will prompt for approval of each hook script
at first use. Accept once, or pre-approve via `HERMES_ACCEPT_HOOKS=1` or
`hooks_auto_accept: true` in config.

### Step 3: Start Tender supervising Hermes

```bash
# Start the Hermes CLI under Tender
# --stdin enables PTY/pipe control
# --replace kills any existing session of same name
# --namespace hermes groups all sessions
tender start --stdin --name hermes-cli --namespace hermes -- hermes

# In another terminal: subscribe to the stream
tender watch --namespace hermes --events --logs
```

**Remote host (e.g., Dreyfus with GPU):**

```bash
ssh dreyfus 'tender start --stdin --name hermes --namespace hermes -- hermes'
# Observe from local machine:
tender --host dreyfus watch --namespace hermes --events
```

### Step 4: Verify events land in Tender

Run a Hermes session that calls a tool, then:

```bash
tender list --namespace hermes
tender log hermes-cli -r | tail -20
tender watch --namespace hermes --json | jq 'select(.type == "block.tool")'
```

### Step 5: Use downstream consumers

#### Example: dashboard query

```bash
# All tool calls with duration >5s
tender watch --namespace hermes --json \
  | jq 'select(.type == "block.tool" and .metrics.duration_ms > 5000)'
```

#### Example: crash bundle on `post_tool_call` failure

If a tool errors (Hermes captures the error in the `result` field), the
bridge can emit an additional event or a script can:

```bash
tender bundle <block_id> --with-causal-ancestors --output hermes-fail.tgz
```

## Why Shell Hooks, Not a Python Plugin

Three reasons to prefer the shell-bridge pattern for this integration:

1. **Zero coupling.** Hermes does not import Tender code. Tender does not
   import Hermes code. The interface is the stdin JSON of one subprocess.
   This is exactly the pattern the `tender-as-block-runtime.md` spec
   describes.

2. **Hermes plugin surface is in-process.** Python plugins run inside
   `hermes-agent`'s Python process. Installing a Tender Python plugin would
   require importing Tender's Rust core via FFI or embedding a subprocess
   call anyway. Shell hooks avoid the complexity.

3. **Tender's event emit primitive is designed for exfiltration from
   supervised processes.** The `--json` flag accepts NDJSON on stdin. Any
   language that can write to stdout can emit. Shell bridge is the minimum
   viable producer.

A native Python plugin could later replace the shell script for sub-millisecond
latency if `tender event emit` gets a fast local IPC path (Unix socket,
Cap'n Proto). For current Tender architecture, shell is correct.

## Schema Versioning

The bridge emits `schema_version: 1`. Future bumps only when Tender's core
block schema changes:

| Version | What changed |
|---|---|
| 1 | Initial: `type`, `block_id`, `timestamp`, `provenance`, `spec`, `lifecycle`, `metrics`, `payload` |

Downstream consumers should gate on `schema_version >= 1` and ignore unknown
fields (forward compat).

## Boundary Discipline

This integration must not violate Tender's layer 1-3 ownership.

| Tender does | Tender does NOT |
|---|---|
| Record block events emitted by the bridge | Decide whether a tool is safe |
| `watch --namespace hermes` streams events | Retry failed tools |
| Content-address I/O (CAS) for captured output | Parse Hermes conversation history |
| Cross-host execution (`--host`) | Choose which model Hermes uses |

| Hermes does | Hermes does NOT |
|---|---|
| Invoke tools, measure latency, capture results | Store durable event logs |
| Emit hook payloads | Parse Tender's NDJSON back into its own state |
| Veto/block dangerous operations | Decide event-stream namespaces |

Both tools keep their jobs. The bridge is stateless glue.

## Platform Notes

### macOS (local)
- `uuidgen` available. `jq` via `brew install jq`.
- Tender daemon runs on local machine. Bridge calls `tender` binary
  directly (no `--host`).

### Linux (Dreyfus, VMs)
- `uuidgen` via `apt install uuid-runtime`. `jq` via `apt install jq`.
- Bridge should check for `tender` on PATH; if missing, use absolute path
  `~/.local/bin/tender`.

### Windows
- No `uuidgen` or bash by default. Ship a `.ps1` bridge instead:

```powershell
# tender-emit.ps1 (PowerShell)
$payload = $input | ConvertFrom-Json
$eventType = switch ($payload.hook_event_name) {
    "on_session_start"    { "block.start" }
    "post_tool_call"      { "block.tool" }
    "on_session_end"       { "block.end" }
    default                { "agent.event" }
}
$blockId = [Guid]::NewGuid().ToString()
$envelope = @{
    schema_version = 1
    type = $eventType
    block_id = $blockId
    timestamp = (Get-Date -Format "yyyy-MM-ddTHH:mm:ssZ")
    provenance = @{ origin = "Agent"; actor = "Hermes"; host = $env:COMPUTERNAME }
    metrics = @{ duration_ms = $payload.extra.duration_ms }
    payload = $payload
} | ConvertTo-Json -Depth 10 -Compress
$envelope | tender event emit --namespace "hermes-$($payload.session_id)" --json -
```

### Remote host: the `--host` split

`--host`-supported commands on Tender: `start`, `status`, `list`, `log`,
`kill`, `wait`, `watch`, `attach`. **NOT supported: `exec`, `run`, `wrap`,
`prune`**. The bridge calls `tender event emit` which will be a local-only
command until Tender supports `--host` for it. For remote Hermes hosts:
- Start via `tender --host remote start ...`
- The bridge on the remote host calls the *local* `tender event emit`
  (tender is installed on the remote host)
- Watch/audit from local machine via `tender --host remote watch ...`

## Failure Recovery

| Symptom | Cause | Fix |
|---|---|---|
| Bridge script not found | `~/.local/bin/` not on PATH | Use absolute path in `hooks:command` |
| `tender event emit` fails with "unknown command" | Tender version too old | Upgrade to Tender >=0.3.0 (after `event-emit-primitive` ships) |
| Hook fires but no events in `watch` | Bridge exited non-zero before calling emit | `chmod +x tender-emit.sh`, check `jq` installed |
| Events appear but all type="agent.event" | `hook_event_name` field missing from payload | Verify Hermes >=v0.14.0 (field added in that version) |
| `tender watch` shows no namespace | Namespace not set on start | `tender start --namespace hermes ...` |

## Acceptance Criteria

- [ ] `tender watch --namespace hermes --json` shows events within 1 second of each Hermes tool call
- [ ] `tender list --namespace hermes` renders the Hermes session as a block
- [ ] Bridge handles all `VALID_HOOKS` events without dropping unknown ones
- [ ] Block IDs are stable/stably generatable (UUIDv4)
- [ ] `duration_ms` from Hermes maps to `metrics.duration_ms` in Tender event
- [ ] Works on macOS (Rick's machine) and Linux (Dreyfus)
- [ ] First-run consent documented (Hermes prompts for hook approval)

## Open Questions

1. **Should the bridge also emit pre_tool_call block attempts?**
   `pre_tool_call` can veto tools. Emitting a `block.start` event that is
   later superseded by nothing (if blocked) leaves an orphan block in the
   stream. Recommend: emit `block.attempted` with `lifecycle.state:
   "canceled"` when vetoed, else the causal chain is broken.

2. **Should Hermes agent_version be in provenance?**
   The bridge currently reads `${HERMES_VERSION:-unknown}` from env. Hermes
   does not export its version to the hook environment. Recommend: Hermes
   adds `HERMES_VERSION` to the shell hook env, or the bridge shells out to
   `hermes --version` once at startup.

3. **Should the bridge buffer and batch events?**
   Hermes can fire many `post_tool_call` events per second. `tender event
   emit` is a subprocess call per event. If latency matters for UI consumers,
   a small `--batch-window-ms` flag or a local Unix-socket daemon bridge
   could batch writes. Defer until profiling shows a problem.

4. **What about gateway dispatch hooks (`pre_gateway_dispatch`)?**
   These fire inside the Hermes gateway (Telegram/Discord) path. They carry
   `event: MessageEvent` which may contain PII. Recommend: map
   `pre_gateway_dispatch` to a `block.gateway` type but redact message text
   in the payload. Or skip it entirely if audit sensitivity matters.

## Depends On

- `event-emit-primitive` — Tender must ship `tender event emit` before this
  integration is executable.
- `skill-claude-code` — the skill should reference this integration pattern
  as the canonical Tender bridge for any agent with hooks, not just Claude Code.

---

## Integration as a Tender Backlog Item

This plan positions Hermes as a *validated consumer* of Tender's block-runtime
protocol — proving the protocol works for non-Claude-Code agent runtimes.
Once `event-emit-primitive` ships, this becomes the canonical example in
Tender's README for "how to integrate your agent."
