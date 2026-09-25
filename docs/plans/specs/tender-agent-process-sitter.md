# Tender — Agent Process Sitter

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** A cross-platform Rust CLI that lets AI agents start, observe, chain, and kill supervised runs — locally or over SSH — without ever needing an interactive terminal. Tender is the execution substrate under agent workflows, hook wrappers, and future frontends — not a frontend itself.

**Tech Stack:** Rust, serde_json, clap, cross-compiled to static binaries (musl on Linux, native on macOS/Windows).

**Lineage:** Successor to `atch` (C, Unix-only, PTY-first). Keeps the agent workflow (`start → log → push → kill`) but redesigns for agents-first, cross-platform, structured output, and composition primitives.

---

## The Model

**Tender models supervised runs, not processes. The sidecar is the supervisor. The OS backend only provides spawn, wait, identity, and tree-kill.**

```
Tender = stateless CLI + durable session record + per-session supervisor + OS-native kill/wait
```

Tender is **not**: a daemon, a portable systemd, a shell wrapper, a PTY/session manager, or a parent-PID-tree inspector.

### Core Abstraction: the Run

| Concept | Role | Lifetime |
|---------|------|----------|
| **Session name** | Human-stable handle (e.g. `upload`) | Reusable across runs |
| **run_id** | Globally unique execution identity (UUID v7). **The authoritative identifier for a run.** All dependency binding, webhook payloads, log correlation, and remote references use run_id. | Immutable per execution |
| **Generation** | Monotonic counter per session name. Human-readable, useful for debugging and detecting reuse. Not used for lifecycle decisions. | Increments on `--replace` or restart |
| **Sidecar** | Sole authority for lifecycle transitions | Born with run, dies with run |
| **Child** | Opaque OS process being supervised | Owned by sidecar |
| **meta.json** | Durable state snapshot | Survives reboot |
| **output.log** | Durable observability stream (line-oriented) | Survives reboot |
| **CLI** | Pure stateless client — transactional operations, then exits | Ephemeral |

### Architectural Layers

Tender has one lifecycle model and multiple access paths.

| Layer | Responsibility |
|------|----------------|
| **Tender core** | Run model, state machine, sidecar, session store, log store, canonical event schema |
| **Semantic backend API** | `start`, `status`, `list`, `log`, `push`, `kill`, `wait`, `watch` |
| **Backend implementations** | Local first; SSH-backed remote later |
| **Helper infrastructure** | Bootstrap, auth, connection reuse, optional broker/relay |
| **Orchestration** | Fanout over one or more backends |
| **Human mode** | PTY attach/detach, explicitly secondary |

Tender is substrate, not frontend. Agent frameworks, hook wrappers, CI systems, and interactive tools are consumers of the semantic backend API. Tender itself never decides what to run or when — it provides the supervised execution, structured output, and composition primitives that callers build on.

The important boundary is the **semantic backend API**, not raw packet transport.

- Local execution should call Tender core directly.
- Remote execution may invoke remote `tender` over SSH.
- Any future broker/relay is helper infrastructure below the backend boundary, not a second lifecycle system.

### Invariants

1. **Sidecar is sole writer of lifecycle state.** CLI never "helpfully" writes lifecycle conclusions. If the sidecar didn't write it, it didn't happen.
2. **run_id is globally unique.** Safe for remote references, log correlation, webhook payloads. UUID v7 (time-sortable).
3. **run_id binds dependencies.** `--after job1` captures job1's current run_id at bind time. If job1 is replaced (new run_id), the dependency fails rather than observing a different execution.
4. **Crash recovery is explicit.** If sidecar disappears without writing terminal state (reboot, OOM-kill, kernel panic), `tender status` detects the released lock + missing terminal state and writes `sidecar_lost`. This is the only case where CLI writes state — and it's a reconciliation, not a lifecycle transition.
5. **`--replace` is atomic.** Uses lock acquisition to serialize. Two agents racing `--replace` on the same session: one wins the lock, kills the old run, starts the new one. The other blocks on the lock, then sees the new run and gets a conflict error (different launch spec) or returns it (same spec). No dual-winner.
6. **Remote is backend access, not a second lifecycle model.** The local and remote paths expose the same semantic operations and the same event model. `--host` is a CLI affordance over a remote backend, not the architecture itself.
7. **Broker/relay is helper infrastructure only.** If introduced later, it may help with bootstrap, connection reuse, auth, or persistent streams, but it must not invent a separate run model, state machine, or event schema.

### Launch Spec

The full identity of a run. Stored in `meta.json`, hashed for idempotent matching:

```json
{
  "argv": ["rclone", "copy", "src/", "dest/"],
  "cwd": "/data/disk1",
  "env": {"RCLONE_TRANSFERS": "16"},
  "timeout_s": 3600,
  "after": [{"session": "extract", "run_id": "019..."} ],
  "namespace": "upload-batch-42",
  "on_exit": ["file:/tmp/done"],
  "stdin_mode": "pipe"
}
```

### meta.json Schema

Version 1. Schema version is persisted for future migrations.

```json
{
  "schema_version": 1,
  "session": "upload",
  "run_id": "01958a3b-...",
  "generation": 3,
  "launch_spec_hash": "sha256:abc...",
  "launch_spec": { ... },
  "sidecar_pid": 1234,
  "sidecar_start_time_ns": 1710612345000000000,
  "child_pid": 1235,
  "child_start_time_ns": 1710612345100000000,
  "state": "running",
  "exit_code": null,
  "started_at": "2026-03-16T10:00:00Z",
  "ended_at": null,
  "restart_count": 0
}
```

### Cross-Platform Map

The portable thing is "one supervisor owns one run." The OS backend is just plumbing:

| Capability | Linux/macOS | Windows |
|-----------|-------------|---------|
| Spawn + detach | fork + setsid | CreateProcess + DETACHED_PROCESS |
| Identity | (pid, /proc starttime or kinfo_proc) | (pid, GetProcessTimes) |
| Wait | waitpid | WaitForSingleObject |
| Tree kill | kill(-pgid) | TerminateJobObject |
| Tree containment | process group (setpgid) | Job Object |

### Platform Abstraction Layers

Three layers, clean separation. Only the first is the `Platform` trait.

**1. Platform — runtime primitives (the trait)**

Process identity, child containment, stdin transport, readiness channel. Used by sidecar/runtime code.

Containment is already behind `SupervisedChild` + `ChildKillHandle`. The implementation evolves per-OS (process groups → cgroups → Job Objects) without changing the trait:

```
type SupervisedChild;
type ChildKillHandle;       // Send + Clone, carries whatever context the OS needs
fn spawn_child(...);        // Sets up containment (setpgid, Job Object, etc.)
fn child_kill_handle(...);  // Extracts kill context from child
fn kill_child(...);         // Tree kill via whatever containment the OS provides
```

No `ProcessContainer` type needed. The abstraction is already right.

**2. WatchBackend — filesystem change observation (future trait)**

Not part of Platform. Used by CLI/query-plane code (`watch`, `wait`), not by sidecar.

```
trait WatchBackend {
    type Watcher;
    fn watch_session_tree(path: &Path) -> io::Result<Self::Watcher>;
    fn wait_for_change(w: &mut Self::Watcher, timeout: Duration) -> io::Result<Vec<ChangeEvent>>;
}
```

| Platform | Mechanism |
|----------|-----------|
| Linux | inotify |
| macOS | kqueue |
| Windows | ReadDirectoryChangesW |

Phase 2B uses polling. WatchBackend is an optimization seam for later — the polling implementation is correct, bounded, and sufficient for v0.

**3. Integrations — OS service and notification features (out of scope)**

Not in Platform. Not in WatchBackend. These are optional adapters that live in a separate `integrations/` layer if ever needed:

- **sd_notify** — systemd readiness. Linux-only, service-manager integration, not needed for Tender's core model.
- **Desktop notifications** — macOS (`osascript`), Linux (`notify-send`), Windows (toast). These are `on_exit` callback concerns or frontend concerns, not supervision primitives.
- **Terminal escapes** — OSC 9 etc. Consumer-side, not Tender's job.

Tender emits durable state and watch events. Consumers decide how to surface them.

### Retention and GC

Durable sessions accumulate forever unless pruned. Policy:

- `tender prune --older-than 30d` — delete session dirs with terminal state older than threshold
- `tender prune --namespace ci-42` — delete all sessions in a namespace
- No automatic GC. Agents or cron call `prune` explicitly. Silent data deletion is not agent-friendly.

### Event Model

Tender has one event envelope shared across all event kinds. The envelope is frozen — new event kinds may be added, but the envelope shape does not change.

#### Envelope

```json
{
  "ts": 1774651234.123456,
  "namespace": "ws-1",
  "session": "claude-1",
  "run_id": "019...",
  "source": "tender.sidecar",
  "kind": "run",
  "name": "run.exited",
  "data": {
    "reason": "ExitedOk",
    "exit_code": 0
  }
}
```

Fields:

| Field | Type | Description |
|-------|------|-------------|
| `ts` | float | Epoch seconds with microsecond precision |
| `namespace` | string | Session namespace |
| `session` | string | Session name |
| `run_id` | string | UUID v7 of the current execution |
| `source` | string | Dotted identifier of the event producer |
| `kind` | string | Event category: `run`, `log`, or `annotation` |
| `name` | string | Specific event name within the kind |
| `data` | object | Kind-specific payload |

No global sequence number. Ordering is by stream order plus timestamp. Source-local sequencing (`source_seq`) may be added later if needed.

#### Event Kinds

**`run` — canonical lifecycle truth.** Produced only by Tender sidecar. These are supervision facts.

| Name | Data | When |
|------|------|------|
| `run.started` | `{"status": "Running"}` | Child spawned, sidecar confirmed identity |
| `run.exited` | `{"reason": "ExitedOk", "exit_code": 0}` | Child exited normally |
| `run.killed` | `{"reason": "Killed"}` | Graceful kill completed |
| `run.killed_forced` | `{"reason": "KilledForced"}` | Force kill completed |
| `run.timed_out` | `{"reason": "TimedOut"}` | Timeout enforcement killed child |
| `run.spawn_failed` | `{"error": "..."}` | Child failed to exec |
| `run.sidecar_lost` | `{}` | Reconciliation detected crashed sidecar |

**`log` — observability.** Produced by Tender sidecar. First-class but not lifecycle truth.

| Name | Data | When |
|------|------|------|
| `log.stdout` | `{"content": "..."}` | Line captured from child stdout |
| `log.stderr` | `{"content": "..."}` | Line captured from child stderr |

**`annotation` — external meaning.** Produced by external actors (Claude hooks, cmux, adapters). Explicitly not supervision truth.

| Name (examples) | Source (examples) | When |
|-----------------|-------------------|------|
| `hook.claude.pre_tool_use` | `external.claude_hook` | Claude Code hook fires |
| `hook.claude.notification` | `external.claude_hook` | Claude Code notification |
| `agent.waiting_for_input` | `external.cmux` | App-derived state |

#### Source Convention

Dotted prefix, not an enum:

- `tender.*` — reserved for Tender core. Only `tender.*` may emit `run` events.
- `external.*` — external producers. May only emit `annotation` events.

This is the safety boundary: apps cannot forge lifecycle truth.

#### Annotation Ingestion: `tender wrap`

Agents emit hook events via stdin/stdout JSON (Claude Code, Codex, Cursor, Cline, GitHub Copilot all use this pattern). Tender does not modify agents or their hook configurations. Instead, Tender provides a transparent wrapper that taps the wire:

```bash
tender wrap --session claude-1 --namespace ws-1 --source claude_hook -- <hook-command> [args...]
```

**`wrap` is the public annotation primitive. There is no public `emit` command.**

> **Superseded 2026-07-06:** the no-public-`emit` policy, the watch
> envelope, and the annotation-ingestion event model in this section are
> superseded by [event-protocol.md](event-protocol.md), which adds a public
> `tender emit` (daemonless O_APPEND to the per-session event log, granular
> exit codes, `--best-effort`) alongside `wrap`, now sugar over the same
> append. The forgery concern below is answered differently: reserved
> `kind` prefixes (`run.` etc.) are rejected for user-supplied kinds, so
> lifecycle truth still cannot be forged; arbitrary events are attributed
> via `source`/`writer` rather than prevented. The provenance argument for
> `wrap` stands — wrap events carry observed execution data — it is no
> longer exclusive.

Why `wrap` over `emit`:

- **Provenance.** `wrap` observed the actual execution — stdin payload, stdout response, stderr, exit code. `emit` would only carry "some process asserted this happened."
- **Discipline.** A public `emit` makes it trivial to forge arbitrary annotations. `wrap` constrains annotation production to actual observed command executions.
- **Integration path.** The real problem is: a hook command already exists, you want to tap it. `wrap` is exactly that.

If internal annotation ingestion is ever needed (SDK integrations, trusted local adapters), it should be a library-internal function or hidden `_emit_annotation` subcommand, constrained to `annotation` kind and `external.*` sources only. Never public until justified by a real need.

**Behavior:**

1. Tees stdin to the wrapped command (live stream, not buffer-then-forward — the wrapped hook may expect incremental reads)
2. Captures a copy of stdin for the annotation payload
3. Lets the wrapped command run to completion
4. Captures stdout, stderr, and exit code from the wrapped command
5. Emits an `annotation` event to the session's event log
6. Returns the wrapped command's stdout to the agent's stdout, stderr to stderr, and exits with the wrapped command's exit code

The agent does not know Tender exists. The hook script does not know Tender exists.

**Annotation payload shape:**

```json
{
  "hook_stdin": { ... },
  "hook_stdout": { ... },
  "hook_stderr": "...",
  "hook_exit_code": 0,
  "command": ["cmux", "claude-hook", "pre-tool-use"],
  "truncated": false
}
```

- `hook_stdin`: the JSON payload from the agent (parsed if valid JSON, raw string otherwise)
- `hook_stdout`: the JSON response from the hook script (parsed if valid JSON, raw string otherwise)
- `hook_stderr`: captured stderr as string (may be truncated)
- `hook_exit_code`: integer exit code
- `command`: argv of the wrapped command
- `truncated`: true if any field was truncated due to size limits

**Truncation policy:** TBD. Large payloads (e.g. transcript dumps) should not bloat the event log. A reasonable default is 64KB per field, with `truncated: true` set when hit.

**Example — cmux integration:**

cmux's Claude wrapper currently injects:

```json
{"hooks":{"PreToolUse":[{"type":"command","command":"cmux claude-hook pre-tool-use"}]}}
```

With Tender wrapping:

```json
{"hooks":{"PreToolUse":[{"type":"command","command":"tender wrap --session claude-1 --namespace ws-1 --source claude_hook -- cmux claude-hook pre-tool-use"}]}}
```

cmux still gets its hook call. Tender also gets the event in the watch stream. No changes to Claude Code. No changes to cmux's hook handler.

#### Phasing

- **Phase 2B:** `run` and `log` kinds only. Source is `tender.sidecar`. No annotations, no `wrap`, no global sequencing.
- **Later:** `tender wrap` for annotation ingestion, `--annotations` flag on watch, `external.*` sources. No public `emit`.

---

## Positioning

Tender sits at the **process supervision** layer — below agent internal hooks and above OS process primitives.

```
┌─────────────────────────────┐
│  UI / Frontend              │  AG-UI, cmux, AgentDeck
├─────────────────────────────┤
│  Agent Internal Hooks       │  Claude Code, Codex, Cursor, Cline
├─────────────────────────────┤
│  Process Supervision        │  ← Tender
│  Event Protocol             │
├─────────────────────────────┤
│  OS Process Primitives      │  pid, signals, waitpid, Job Objects
└─────────────────────────────┘
```

### Why this layer is empty

Every AI coding agent has converged on the same internal hook pattern: stdin JSON → command → stdout JSON + exit code. Claude Code (12+ events), Codex (5, feature-flagged), Cursor (7), Cline (4), GitHub Copilot (2+). All proprietary schemas, different field names — but identical transport: stdin JSON, stdout JSON, exit code 2 to block.

Every external tool that wraps agents (AMUX, Batty, AgentDeck) resorts to terminal output scraping because there is no standard way to observe agent process lifecycle externally. This is fragile and breaks when output formats change.

Nobody owns the layer between "what the agent thinks internally" and "is the process alive."

### What Tender provides

| Problem | Current state of the art | Tender |
|---------|--------------------------|--------|
| Is the agent alive? | Scrape tmux / poll pane_dead | ProcessIdentity + lock-based liveness |
| Is the agent ready? | Wait and hope | Readiness handshake over pipe |
| What happened? | Parse ANSI escape codes | `watch` NDJSON event stream |
| PID reuse after restart? | Hope it doesn't collide | (pid, start_time_ns) verified before kill |
| Group related agents? | Ad-hoc session names | `--namespace` |
| Send input to running agent? | tmux send-keys (unstructured) | FIFO stdin push |
| Agent exited — now what? | 5-second poll loop discovers it | `on_exit` callback fires immediately |
| Capture agent hook semantics? | Build a custom bridge per agent | `tender wrap` taps the wire |

### Agent execution backend

Every AI coding agent independently rebuilds the same process management primitives — persistent shell, output capture, timeout/kill, background work, CWD persistence — all incomplete, all tightly coupled to the agent's session lifecycle, all with no orphan recovery.

| What agents reinvent | Claude Code | Codex | Cline | Goose | SWE-agent |
|---------------------|------------|-------|-------|-------|-----------|
| Persistent shell | Pipes + sentinel | PTY + LRU cache | VS Code terminal | subprocess/cmd | Docker exec session |
| Output capture | Reader threads + queues | yield_time_ms polling | OSC 633 markers | Blocking read | communicate() |
| Timeout/kill | 2min/10min auto-bg | per-cmd timeout_ms | None | None | 25s + interrupt |
| Background work | Temp file + notify | PTY session cache | "Proceed While Running" | None | None |
| Orphan recovery | None (leaks 400MB/agent) | None (PTY sessions leak) | None | None | Docker kills all |

Tender replaces the bottom half of every agent's execution stack:

**What agents keep:** tool routing, LLM context management, sandboxing policy, permission UI.

**What Tender provides:**
- Persistent supervised shell (`start --stdin`)
- Framed command execution (`exec` = `push` + `wrap`)
- Output capture with timestamps (`log`)
- Timeout + graceful kill (`--timeout`, `kill`)
- Background work that survives agent crashes (sidecar)
- Orphan recovery (`ProcessIdentity` + reconciliation)
- CWD/env persistence (the shell lives in Tender, not the agent)
- Event stream for any consumer (`watch`)

Tender does not handle sandboxing. That stays with the agent or its host (seatbelt, bubblewrap, Docker, restricted tokens). Tender runs inside or alongside the sandbox — it is the process lifecycle layer, not the security policy layer.

The integration path for any agent:
1. Agent calls `tender start` instead of spawning its own persistent shell
2. Agent sends commands via `tender exec` instead of writing to a private stdin pipe
3. Agent reads output via `tender log` instead of its own capture threads
4. If agent crashes, `tender` keeps the shell alive — agent reconnects via `tender list` + `tender exec`
5. Terminal/orchestrator consumes `tender watch` instead of scraping

### What Tender standardizes

- Run/session identity: namespace, session, run_id
- Canonical event envelope (frozen shape, extensible kinds)
- Canonical run and log events
- Source naming convention (tender.* reserved)
- Annotation carriage (later, via wrap)

### What Tender does not standardize

- Hook names across agents (Claude's PreToolUse ≠ Cursor's beforeShellExecution)
- Hook payload schemas across agents
- Approval/permission semantics
- UI-derived state like "needs input" or "thinking"
- Agent-specific control behavior

If Tender tries to unify agent hook semantics, it will fail. If it defines the universal supervised-run and event envelope, it has a real shot.

### What Tender does not do

- Replace agent internal hooks (Claude Code hooks, Cursor hooks, etc.)
- Replace frontend rendering protocols (AG-UI)
- Provide an agent orchestration framework (that's above Tender)
- Require any agent modification (wrap is transparent)
- Handle sandboxing (that's the agent's or host's job)

### Precedents

- **Quine** (arXiv:2603.18030, March 2026) — academic paper proposing LLM agents as native POSIX processes. Identity = PID, interface = stdin/stdout/stderr + exit status. Tender is the practical implementation.
- **Claude Code hooks** — the current hook-command pattern. Tender carries these as annotations, does not replace them.
- **Aider notifications** — "keep notifications as a separate command hook" pattern. Tender's `on_exit` is the supervision-native version.
- **Ghostty terminal escapes** — terminal-native notification delivery. Tender is transport-agnostic; terminals consume the watch stream however they want.
- **AMUX/Batty/AgentDeck** — demonstrate the demand for external agent supervision. All resort to scraping because no supervision event protocol exists.

---

## Design Rules

These are non-negotiable. Every PR, every feature, every decision filters through these.

### 1. Structured Output is the Only Output

```
tender start job cmd  →  {"session":"job","pid":1234,"state":"running"}
tender status job     →  {"session":"job","state":"exited","exit_code":0,"duration_s":3600}
tender list           →  [{"session":"job","state":"running","age_s":120}, ...]
```

Human-readable is a flag (`--human`, `-H`), not the default. Agents are the primary consumer.

**Rule: if an agent has to regex-parse your output, you failed.**

### 2. Exit Codes are a Contract

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Session error (not found, already exists) |
| 2 | Process error (command failed to start) |
| 3 | Sidecar lost (supervisor crashed; the child may still be running) |
| 4 | Dependency failed (`--after` target exited non-zero) |
| 5 | Sidecar failed (supervisor failed, killed the child, and recorded it) |
| 10 | Remote transport error (SSH failed) |
| 42 | Process exited non-zero (actual code in JSON output) |

**Rule: agents branch on integers, never on string matching.**

### 3. Idempotent Everything

- `start job cmd` when `job` is running with matching launch spec → return existing session, exit 0
- `start job cmd2` when `job` is running with different launch spec → exit 1, session conflict error
- `start job cmd --replace` → kill existing, start new, increment generation (explicit opt-in)
- `kill job` when `job` is already dead → exit 0
- `kill job` when `job` never existed → exit 0

The **launch spec** includes: command, args, cwd, environment overrides, timeout, after-dependencies, and on-exit hooks. All are hashed and stored in `meta.json`. Idempotent match requires the full spec to agree — not just the command name.

**Rule: retrying any command must be safe. Conflicting specs must be loud.**

### 4. Sessions are State Machines

```
starting → spawn_failed
starting → running → exited_ok          (code 0)
                   → exited_error(code)  (code != 0)
                   → killed              (SIGTERM / cooperative)
                   → killed_forced       (SIGKILL / TerminateJobObject)
                   → timed_out           (--timeout exceeded)
                   → sidecar_failed      (sidecar failed, killed the child, said so)
                   → sidecar_lost        (sidecar crashed, detected lazily)
```

State is queryable via `tender status`. Agents branch on the state enum, never infer from output. Terminal states are durable in `meta.json`.

### 5. Composition Over Scripting

```
tender start job2 cmd --after job1              # wait for job1 exit 0
tender start job2 cmd --after job1 --any-exit   # wait regardless of exit code
tender start job cmd --timeout 3600             # kill after 1h
tender start job cmd --on-exit webhook:URL      # notify when done
tender start job cmd --on-exit file:/tmp/done   # touch file when done
```

**Rule: if an agent needs a wrapper script, the API is missing a primitive.**

### 6. Addressable Across Hosts

```
tender --host nas-01 start upload cmd
tender --host nas-01 log upload
tender status nas-01:upload
```

Host registry comes from SSH config or later backend-specific discovery.

The CLI should hide transport details from the caller. The agent should not construct `ssh host tender ...` strings manually.

The architectural point is:

- `--host` is a frontend affordance
- the real abstraction is a remote backend exposing the same semantic Tender API

### 7. Log is Queryable

```
tender log job                    # raw stdout/stderr
tender log job | rg "ERROR"       # pipe to standard search tools
tender log job --since 5m         # time-windowed
tender log job --tail 50          # last N lines
tender log job --follow           # stream
```

**Rule: don't make agents download 100MB of logs to search for one line.**

### 8. Fan-Out is Native

```
tender fanout "roost-*" -- df -h /data
→ [{"host":"roost-01","session":"fanout-abc-01","state":"exited","exit_code":0}, ...]
```

Fanout is orchestration over many backends. It is not part of transport internals.

### 9. No Interactive Anything in the Core Path

`attach` is the only command needing a PTY. Everything else is fire-and-forget. Even `push` is non-interactive.

### 10. Observability Without Polling

```
tender wait job                          # block until exit
tender wait job --timeout 60             # with timeout
tender wait --any job1 job2 job3         # first to finish
tender wait --all job1 job2 job3         # all finish
```

### 11. Namespacing

```
tender start --namespace ci-42 build cmd
tender list --namespace ci-42
tender kill --namespace ci-42 --all
```

### 12. Human Escape Hatch

`attach` exists. `--human` flag exists. But these are secondary to the agent path.

---

## Session Storage

No central daemon. Each session is a directory with a **sidecar process**:

```
~/.tender/sessions/
  upload/
    meta.json       # schema v1, full run identity and state (see Model section)
    output.log      # line-oriented observability stream (see Log Format)
    stdin.pipe      # mkfifo (Unix) / named pipe \\.\pipe\tender-<session> (Windows)
    lock            # flock / LockFileEx — owned by sidecar while running
```

### The Sidecar

`tender start job cmd` does this:

1. CLI creates session directory and writes initial `meta.json` (state: `starting`)
2. CLI spawns **sidecar** — a detached `tender _sidecar` process (same binary)
3. CLI **blocks** until sidecar signals readiness via a pipe/eventfd
4. Sidecar acquires session lock, waits for `--after` dependencies if any
5. Sidecar spawns **child** (the actual command)
6. Sidecar writes `meta.json` (state: `running`, child pid, start_time_ns, generation)
7. Sidecar signals readiness back to CLI
8. CLI reads final `meta.json`, prints JSON output, exits

If the sidecar fails to start or the child fails to spawn, the sidecar writes a terminal state (`spawn_failed`) to `meta.json` and signals the CLI. The CLI exits with the appropriate error code. **`tender start` never returns success for a half-initialized session.**

The sidecar then runs independently:
- Captures child stdout/stderr into `output.log` with timestamps and stream tags
- Writes exit code to `meta.json` when child exits
- Enforces `--timeout` (kills child if exceeded)
- Fires `--on-exit` hooks after child exits
- Releases lock and dies after all post-exit work is done

The sidecar is not a daemon — it's born with the session and dies with it. It's the same binary invoked internally, so there's nothing extra to install.

### Run Identity

Each execution gets a unique **run_id** (UUID v7) and an incremented **generation** counter. run_id is the authoritative execution identity used for all lifecycle decisions. Generation is a human-friendly counter for debugging.

`--after job1` captures job1's current **run_id** at bind time. If job1 is replaced mid-wait (new run_id), the sidecar detects the mismatch and fails with exit code 4 (dependency failed) rather than silently observing a different execution. Generation is not used for this check — run_id is sufficient and globally unique.

### Process Identity

PID reuse is a real problem on long-running systems. A session identifies its child by `(pid, start_time_ns)`:
- Unix: `start_time_ns` from `/proc/<pid>/stat` field 22 (starttime) or `kinfo_proc` on macOS
- Windows: `GetProcessTimes` → `lpCreationTime`

`is_alive` checks both PID existence and birth time match. `kill` refuses to signal if birth time doesn't match (PID was recycled). `meta.json` persists both values.

### Log Format

Single `output.log` file with interleaved stdout/stderr. Each line is prefixed by the sidecar at write time:

```
1710612345.123456 O first line of stdout
1710612345.234567 E something on stderr
1710612345.345678 O second line of stdout
```

`O` = stdout, `E` = stderr. Unix epoch with microseconds. This preserves interleaving chronology and enables `--since` and downstream filtering.

**This is a line-oriented observability log, not a byte-exact replay stream.** Partial lines are buffered until newline. Binary output is not faithfully reproduced. `tender log --raw` strips the timestamp and stream tag prefixes for human readability, but does not guarantee byte-identical reproduction of original output. If exact byte replay is needed (e.g. binary protocols), use the child's own file redirection instead of tender's log capture.

### Detachment

- Unix: sidecar is double-forked + setsid. Child is a direct child of the sidecar (not double-forked), so the sidecar can waitpid.
- Windows: sidecar is CreateProcess + DETACHED_PROCESS. Child is created by sidecar with inherited handles into log file. Sidecar uses WaitForSingleObject on child handle.

---

## Platform Abstraction

Three traits, everything else is shared:

```rust
/// Identity of a running process — PID alone is not safe due to reuse.
struct ProcessId {
    pid: u32,
    start_time_ns: u64,  // birth marker, platform-specific source
}

trait ProcessSpawner {
    /// Spawn sidecar as detached process. Returns sidecar's ProcessId.
    fn spawn_sidecar(tender_bin: &Path, session_dir: &Path, cmd: &[String]) -> Result<ProcessId>;
    /// Check if process is alive AND matches the expected birth time.
    fn is_alive(id: &ProcessId) -> bool;
    /// Kill process. Graceful attempts cooperative shutdown first.
    fn kill(id: &ProcessId, graceful: bool) -> Result<()>;
    /// Get birth time for a running PID (for identity verification).
    fn get_start_time(pid: u32) -> Result<u64>;
}

trait StdinPipe {
    /// Create the pipe. On Unix: mkfifo at path. On Windows: named pipe in \\.\pipe\ namespace.
    fn create(session_name: &str, session_dir: &Path) -> Result<Self>;
    fn write(&self, data: &[u8]) -> Result<()>;
}

trait PtySession {  // only for `attach`
    fn open(cmd: &[String]) -> Result<Self>;
    fn resize(rows: u16, cols: u16) -> Result<()>;
    fn detach(self) -> Result<()>;
}
```

| Concern | Unix/macOS | Windows |
|---------|-----------|---------|
| Sidecar spawn | fork + setsid | CreateProcess + DETACHED_PROCESS |
| Child spawn | sidecar fork/exec | sidecar CreateProcess in Job Object |
| Is alive | kill(pid, 0) + check /proc starttime | OpenProcess + GetProcessTimes |
| Kill (graceful) | SIGTERM → sleep → SIGKILL | Job Object: TerminateJobObject (kills entire tree) |
| Kill (tree) | kill process group (-pid) | Job Object handles this natively |
| stdin push | mkfifo in session dir | CreateNamedPipe in `\\.\pipe\tender-<session>` |
| PTY (attach) | openpty/forkpty | CreatePseudoConsole (ConPTY) |
| File lock | flock | LockFileEx |

### Windows Process Model

Windows does not have Unix-style signals. The plan does **not** try to map SIGTERM → GenerateConsoleCtrlEvent (which doesn't work for DETACHED_PROCESS). Instead:

- Child is spawned inside a **Job Object** with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
- **Cooperative shutdown** (not "graceful"): sidecar sets a "stop requested" flag in meta.json, then waits a grace period. Processes that are aware of tender can poll this flag. Most processes will not — this is best-effort, not a general mechanism.
- If child doesn't exit within grace period, Job Object terminates the entire process tree via TerminateJobObject.
- For tree cleanup, this is actually *better* than Unix — Job Objects kill all descendants, not just the immediate child.

The stdin pipe lives in `\\.\pipe\tender-<session>`, not as a file in the session directory. The `StdinPipe` trait abstracts this — callers never see the path difference.

---

## Crate Structure

This is the **target shape**, not the current repository layout.

Current reality:

- Tender is still implemented as a single crate
- the module boundaries in `src/` are the immediate design boundary
- Phase 2B does not require a workspace split

The workspace/crate split should happen when backend and remote pressure make the boundaries concrete enough to justify the churn.

```
tender-cli/
  Cargo.toml          # workspace root
  crates/
    tender-core/      # run model, state machine, session dir, log reader, event schema
    tender-platform/  # ProcessSpawner, StdinPipe, PtySession per OS
    tender-backend/   # semantic backend API + local/remote backend glue
    tender-remote/    # SSH backend, bootstrap hooks, remote execution helpers
    tender-fanout/    # parallel ops over one or more backends
    tender-broker/    # optional helper for connection reuse / bootstrap / persistent streams
  src/
    main.rs           # CLI entry point (clap)
  tests/
    integration/      # end-to-end tests
```

---

## Build Targets

```
cargo build --target x86_64-unknown-linux-musl     # static Linux amd64
cargo build --target aarch64-unknown-linux-musl     # static Linux arm64
cargo build --target aarch64-apple-darwin           # macOS Apple Silicon
cargo build --target x86_64-pc-windows-msvc         # Windows
```

Single binary, no runtime dependencies.

---

## Phased Implementation

### Phase 1: Core (local, Linux/macOS)

The minimum viable agent process sitter. Replaces atch for local use.

1. Scaffold workspace, crates, CI
2. `tender-core`: session directory layout, meta.json schema, state machine types
3. `tender-core`: ProcessId with (pid, start_time_ns) identity
4. `tender-platform` (Unix): spawn_sidecar, is_alive (with birth time check), kill (SIGTERM → SIGKILL), get_start_time
5. Sidecar: internal `_sidecar` subcommand — spawn child, capture output with timestamps, write exit state to meta.json
6. CLI commands: `start`, `status`, `list`, `kill`
7. Log capture: combined output.log with timestamp + stream tag per line
8. CLI commands: `log`, `log --tail`, `log --follow`, `log --raw`
9. stdin pipe: `push` via mkfifo
10. `wait` command (block until exit, poll meta.json)
11. JSON output by default, `--human` flag
12. Exit code contract
13. Idempotent start with launch-spec matching
14. Integration tests

**Deliverable:** drop-in replacement for atch on Linux/macOS, structured output, agent-first.

### Phase 2: Windows

15. `tender-platform` (Windows): CreateProcess + DETACHED_PROCESS, Job Objects for child, GetProcessTimes for identity
16. Windows stdin: CreateNamedPipe in `\\.\pipe\tender-<session>`
17. Windows kill: Job Object termination (not GenerateConsoleCtrlEvent)
18. Windows CI (cross-compile + test on GitHub Actions)
19. Integration tests on Windows

**Deliverable:** same binary, same commands work on rick-windows.

### Phase 3: Composition

All composition features are implemented in the sidecar — no new processes or daemons needed.

20. `--timeout` on start (sidecar kills child after duration, writes `timed_out` state)
21. `--after` dependency chaining (sidecar polls dependency's meta.json before spawning child)
22. `--on-exit` notifications (sidecar fires after child exits: file touch, webhook)
23. `--namespace` for session grouping
24. `kill --namespace --all` cleanup

**Deliverable:** agents can chain work without wrapper scripts.

### Phase 4: Remote Backend

25. Define semantic backend boundary for local and remote execution
26. `tender-remote`: SSH-backed remote backend exposed via `--host`
27. Host resolution from SSH config
28. `host:session` addressing
29. Error classification (SSH fail vs tender fail vs process fail)
30. `tender fanout` with parallel execution and result collection over backends
31. Optional bootstrap hooks for ensuring remote `tender` exists
32. Broker/relay explicitly deferred unless SSH proves insufficient

**Deliverable:** fleet operations from a single command.

### Phase 5: Human Escape Hatch

33. `tender-platform` PTY support (Unix: forkpty, Windows: ConPTY)
34. `attach` command
35. Detach key handling

**Deliverable:** humans can take over when agents can't handle it.

### Phase 6: Skill + Migration

36. Write tender skill for Claude Code / Codex / other agents
37. Migration guide from atch → tender
35. Update fleet: install tender alongside atch, validate, cut over

---

## What Tender Replaces

| Today | Tender |
|-------|--------|
| `ssh host atch start ...` | `tender --host start ...` |
| Wrapper scripts for chaining | `--after`, `--on-exit` |
| `sleep && check` loops | `tender wait` |
| Bash for-loops over hosts | `tender fanout` |
| Searching log output | `tender log \| rg ...` |
| Parsing human text | JSON by default |
| Screen/tmux | `attach` as explicit human mode |
| Broken on Windows | First-class Windows support |

---

## Design Review Resolutions

### Round 1 (architectural)

| # | Finding | Resolution |
|---|---------|------------|
| 1 | No persistent actor for timeout/after/on-exit | **Sidecar process** — lightweight per-session supervisor, same binary, born and dies with session. Not a daemon. |
| 2 | PID-only identity allows reuse collisions | **ProcessId = (pid, start_time_ns)** — birth time from /proc (Linux), kinfo_proc (macOS), GetProcessTimes (Windows). All liveness checks and kills verify both. |
| 3 | GenerateConsoleCtrlEvent doesn't work with DETACHED_PROCESS | **Job Objects** — child spawned inside Job Object. Cooperative shutdown via flag in meta.json + timeout. Tree kill via TerminateJobObject. Better than Unix for descendant cleanup. |
| 4 | Split stdout/stderr loses interleaving | **Single output.log** — sidecar captures both streams, prefixes each line with `<epoch_us> <O\|E>`. Interleaving preserved. |
| 5 | Idempotent start masks command mismatches | **Full launch-spec matching** — idempotent only if full spec matches. Different spec with same session name → exit 1 conflict error. `--replace` for explicit override. |
| 6 | Windows named pipes don't live in session dir | **Acknowledged in trait** — `StdinPipe::create` takes session name, Unix uses session dir path, Windows uses `\\.\pipe\tender-<session>`. Abstraction is honest about the difference. |

### Round 2 (semantic)

| # | Finding | Resolution |
|---|---------|------------|
| 1 | start returns before sidecar is ready | **Readiness handshake** — CLI blocks on pipe/eventfd until sidecar has lock, spawned child, and written meta.json. Never returns success for half-initialized session. |
| 2 | Launch spec too narrow (cmd+args only) | **Full spec hash** — includes cmd, args, cwd, env overrides, timeout, after-deps, on-exit hooks. All stored and compared. |
| 3 | --after binds to name, not execution | **run_id binding** — --after captures target's run_id (UUID v7) at bind time. If target is replaced (new run_id), dependency fails. Generation is a human counter only. |
| 4 | output.log is observability log, not byte replay | **Documented explicitly** — line-oriented, partial lines buffered, binary not faithful. --raw strips prefixes for readability, not byte-exact replay. |
| 5 | Windows "graceful kill" overstated | **Renamed to cooperative shutdown** — best-effort flag polling, most processes won't implement. Documented as such. |

---

## OTP Lessons for Tender

Tender's architecture maps to OTP concepts. Learn structure from OTP, but be more conservative about automatic recovery — OS processes have side effects that make blind restart dangerous.

### Mapping

| OTP | Tender | Notes |
|-----|--------|-------|
| Client process | CLI (`tender start`, `tender status`) | Short-lived, exits after command |
| supervisor / supervisor_bridge | Sidecar (`tender _sidecar`) | Per-session, stateful control loop around one child |
| Worker | Child OS process | The actual command |
| Process state (in-memory) | `meta.json` + `output.log` | Externalized to disk — no BEAM to keep it in memory |
| Registered name | Session name | Mutable binding — can be `--replace`d |
| Child instance identity | run_id (UUID v7) | Prevents name aliasing across runs. Generation is a human-readable counter, not used for lifecycle decisions. |
| Links/monitors | Sidecar waitpid/WaitForSingleObject | Sidecar is sole observer of child lifecycle |

### Exit Classification

OTP distinguishes normal, shutdown, and error exits. Tender must do the same in `meta.json`:

| Exit reason | Meaning | `meta.json` state |
|-------------|---------|-------------------|
| Code 0 | Child completed successfully | `exited_ok` |
| Code != 0 | Child failed | `exited_error(code)` |
| SIGTERM/cooperative | Tender killed it (user or dependency) | `killed` |
| SIGKILL/TerminateJobObject | Force kill after grace period | `killed_forced` |
| Timeout | `--timeout` exceeded | `timed_out` |
| Spawn failure | Command not found, permission denied | `spawn_failed` |
| Sidecar crash | Sidecar died unexpectedly | Detectable: lock released but no terminal state written. Next `status` call writes `sidecar_lost`. |

### Restart Policy

Tender does **not** auto-restart by default. OS process restart is more dangerous than BEAM process restart — environment, cwd, pipes, partial external work, and side effects make blind restart unsafe.

Define policy as a first-class field on `tender start`:

| Policy | Behavior | OTP equivalent |
|--------|----------|----------------|
| `--restart never` (default) | No restart. Terminal state is final. | `temporary` |
| `--restart on-failure` | Restart on non-zero exit, not on kill/timeout | `transient` |
| `--restart always` | Restart on any exit except explicit kill | `permanent` |

A restart is a **new run under the same session name**. Each restart:
- Gets a new **run_id** (new execution identity)
- Increments **generation**
- Spawns a new child process
- The **same sidecar** manages the restart cycle (it does not die and respawn)

This means `--after` dependencies that bound to the previous run_id will correctly fail — they don't accidentally observe a restarted execution.

If restart is enabled:
- **Bounded intensity**: max N restarts in M seconds (default: 3 in 60s). Exceeding this writes `restart_limit` state and stops. No loop bombs.
- **Backoff**: exponential with jitter (1s, 2s, 4s, ..., capped at 60s)
- **Restart history** persisted in `meta.json` as an array of `{run_id, generation, exit_code, started_at, ended_at}`
- **Current run** fields in `meta.json` always reflect the latest attempt

Restart is a Phase 3+ feature. The contract is defined now so the state machine doesn't need redesigning later.

### Principles Adopted from OTP

1. **Separate names from instances.** Session name is a mutable binding. run_id is identity. Generation is a human debugging counter.
2. **Classify exits.** Not just "exited vs killed" — six distinct terminal states.
3. **Startup acknowledgment is strict.** Readiness handshake before CLI returns success.
4. **Supervision is local and boring.** One sidecar, one child, small scope.
5. **Sidecar is sole writer.** No other process writes lifecycle state. Observation without inference.
6. **Persist terminal state.** Write to `meta.json` before sidecar exits. Durable across reboot.
7. **Be more conservative than OTP about restart.** Default is `never`. Restart is opt-in, bounded, and backoff-protected.

---

## Platform Supervisor Landscape

### Decision: single binary, no external manager required

Tender does not depend on systemd, SCM, or any platform supervisor. The sidecar *is* the supervisor. This is deliberate — agents need uniform semantics across Linux, macOS, and Windows. An external manager dependency would fracture that.

### What we steal from each platform

| Platform feature | What it does well | What Tender takes |
|-----------------|-------------------|-------------------|
| **systemd** (Linux) | Readiness notification (sd_notify), exit classification, cgroup tree kill, RuntimeMaxSec, restart policies | Readiness handshake, exit taxonomy, timeout as first-class, restart policy contract |
| **Job Objects** (Windows) | Process tree containment, kill-all-descendants, accounting | **Used directly** — child spawned inside Job Object. This is the Windows implementation, not inspiration. |
| **cgroups** (Linux) | Process tree kill without PID chasing | Sidecar uses kill(-pgid) for process group. Job Objects are the Windows equivalent. |
| **journald** (Linux) | Structured, queryable logs with timestamps | output.log format with timestamp + stream tag per line |
| **launchd** (macOS) | Readiness via `xpc`, process lifecycle management | Nothing directly — macOS sidecar uses same POSIX path as Linux |

### What we don't use and why

| Platform feature | Why not |
|-----------------|---------|
| **systemd as backend** | Linux-only. Not available in containers, minimal distros, WSL, CI. Agents would need two code paths. |
| **SCM / Windows Services** | Too heavyweight — designed for persistent system components, not ad-hoc agent processes. Registration requires admin. |
| **Task Scheduler** | Trigger-based, not supervision. Wrong abstraction. |
| **Event Log / ETW** | Vendor-specific log sink. Tender's output.log is portable and self-contained. |
| **launchd agents/daemons** | plist registration, macOS-only, same portability problem as systemd. |

### Future: optional systemd backend

A `--backend systemd` flag could map to `systemd-run --user` on Linux hosts where it's available. This would give cgroup tree kill and journal integration for free. But it's Phase 6+ at earliest, and agents would still use the same tender CLI — the backend is an implementation detail, not exposed to callers.

---

## Open Questions

- **Crate name:** `tender-cli` on crates.io (binary name `tender`). The `tender` crate (v0.1.1, Raft lib, dead since 2022) is not a blocker but we avoid collision.
- **Repo location:** new repo `rmorgans/tender` or keep in atch repo with rename?
- **atch compatibility:** should tender understand atch session directories for migration, or clean break?
- **Log rotation:** sessions that run for weeks will accumulate huge logs. Built-in rotation or leave to the user?
- **Auth for remote:** SSH keys only, or support SSH agent forwarding, certificates, etc.?
