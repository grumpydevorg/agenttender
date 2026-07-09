# Tender as Block Runtime

**Status:** Accepted — extends positioning  
**Date:** 2026-05-23  
**Naming/schema note (2026-07-06):** the event CLI and schema in this doc
predate [event-protocol.md](event-protocol.md), which is now the schema
owner. Where they disagree, event-protocol.md wins — in particular:
`tender event emit` → `tender emit`; `tender watch --namespace --json` →
`tender events`; `parent_block_id` → `parent_id`; ULID → UUIDv7. A full
reconciliation pass over this doc (roadmap staleness included) is pending;
this note prevents implementation drift until then.

**Supersedes (in part):** [decision-process-sitter-not-framework.md](decision-process-sitter-not-framework.md) — the "no protocol awareness" stance softens to "no LLM-specific protocol awareness; structured event ingestion from supervised processes is now in scope."

## Summary

Tender remains a process supervisor at its floor. It additionally exposes its event stream as the **universal protocol layer for supervised execution events** — a stream that consumers (terminal UIs, dashboards, audit systems, agents) subscribe to, and that supervised processes can publish into via a small additive primitive.

In other words: Tender is still a process sitter. It is *also* a block runtime, where a *block* is any addressable unit of supervised execution — a session, a command inside a shell, a tool call inside an agent, a step inside a test suite.

The previous "no framework" decision held only because no clean mechanism existed for processes to tell Tender what they were doing internally. Hooks change that. Any tool with a lifecycle-hook system (Claude Code, shells via OSC 133, pytest, CI runners, MCP servers) can now publish structured events into Tender's stream without Tender knowing anything tool-specific. This is federation, not framework.

## Context

Three observations from the wider ecosystem motivate this shift:

1. **Warp is now open source (Apache + AGPL+MIT, April 2026).** Their `Block` struct is precisely the addressable command-execution record we'd build — but it's welded to Warp's renderer.
2. **libghostty ships an embeddable VT/grid library** under `<ghostty/vt.h>`. Anyone can build a terminal emulator without re-implementing VT parsing.
3. **Every agent terminal (cmux, AMUX, Batty, AgentDeck) re-implements process supervision badly** — bash wrappers, scattered Swift socket controllers, leaky Go daemons, zombies. They all screen-scrape the terminal to fake structured events Tender already produces.

The gap in the ecosystem is not another terminal emulator (Ghostty has that). It is not another block UI (Warp has that). It is the **protocol layer between supervised execution and any presentation** — the structured event stream that:

- a Warp-style block UI consumes to render
- a CI dashboard consumes to surface failures
- an agent consumes to know what its own tools did
- an audit log consumes to record what was run on which host

Tender already produces most of this stream. The missing piece is letting *supervised processes themselves* publish into it.

## The Block Concept

A **block** is an addressable record of one unit of supervised execution. It has:

- a stable identity (ULID)
- provenance — origin (`Human|Agent|Cron|CI|Api`), actor, host
- a command spec (argv, env, cwd, stdin)
- a lifecycle state (`Queued|Running|Done|Failed|Canceled`)
- timestamps (`created_at`, `started_at`, `completed_at`)
- exit info (code, signal)
- captured I/O (stdout, stderr) — content-addressed; storage is an implementation detail
- annotations (tags, structured metadata, rich content)
- causality — `parent_block_id`, namespace, optional spans

A Tender *session* is a block. A command inside a supervised shell can be a sub-block (parented to the shell session). A tool call inside Claude Code can be a sub-block (parented to the Claude Code session). The relationship is a forest, not a list.

**Linearity is a projection, not a property of the data.** Warp shows blocks in time-order per tab; the same blocks can be projected by host, by tag, by causal lineage, by actor. Tender stores the graph; consumers pick their projection.

## The Five-Layer Stack — Reaffirmed

Tender owns layers 1–3, exactly as [design-principles.md](../../design-principles.md) states.

| Layer | Owns | Examples |
|-------|------|----------|
| 1. Runtime substrate | **Tender** | PTY, child process, kill/wait, sidecar |
| 2. Session control | **Tender** | `start`, `exec`, `attach`, `wait`, `watch` |
| 3. Composition primitives | **Tender** | `--after`, `--on-exit`, namespaces, **event emit (NEW)** |
| 4. Workflow policy | NOT Tender | Retries, health rules, orchestration strategy |
| 5. Domain tools | NOT Tender | Terminal UIs, libghostty integration, AI frameworks |

The block-event protocol is layer 3 — a composition primitive. Anything that interprets blocks semantically (Claude-Code-aware UIs, libghostty shell parsers, OpenTelemetry exporters) is layer 5 and lives outside Tender.

## Where Other Pieces Sit

```
┌──────────────────────────────────────────────────────────────────┐
│  PRESENTATIONS  (layer 5 — separate projects)                    │
│  ┌──────────┬──────────┬──────────┬──────────┐                   │
│  │ Warp-on- │ Wave     │ TUI      │ web      │ ...               │
│  │ Ghostty  │ wsh-like │ blocks   │ board    │                   │
│  │ block UI │ tiles    │          │          │                   │
│  └──────────┴──────────┴──────────┴──────────┘                   │
└──────────────────────────────────────────────────────────────────┘
                              ▲
                              │ subscribe to event stream
                              │
┌──────────────────────────────────────────────────────────────────┐
│  TENDER  (layers 1–3)                                            │
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │ Block API + query layer        (layer 3)                   │  │
│  │   tender watch --namespace --json                          │  │
│  │   tender list --tag … --host … --since …                   │  │
│  │   tender block get <id>  ⟵ NEW: addressable block read     │  │
│  └────────────────────────────────────────────────────────────┘  │
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │ Composition primitives          (layer 3)                  │  │
│  │   --after  --on-exit  --namespace  wrap                    │  │
│  │   tender event emit  ⟵ NEW: in-session event publication   │  │
│  └────────────────────────────────────────────────────────────┘  │
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │ Session control                 (layer 2)                  │  │
│  │   start  exec  push  attach  status  wait  watch  kill     │  │
│  └────────────────────────────────────────────────────────────┘  │
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │ Runtime substrate               (layer 1)                  │  │
│  │   sidecar  PTY/pipe  logs  identity  Win32 Job Objects     │  │
│  └────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────┘
                              ▲
                              │ event intake — multiple channels per agent
                              │
┌──────────────────┬──────────────────┬──────────────────┬────────────────────┐
│ Opaque-byte      │ Hook-emitted     │ OSC 133 via      │ Agent-native       │
│ supervision      │ events           │ libghostty/vt    │ session logs       │
│ (existing)       │ (NEW primitive)  │ (separate crate) │ (consumer reads)   │
│                  │                  │                  │                    │
│ — current Tender │ — Claude Code    │ — `tender-shell` │ — Claude Code's    │
│ — services       │ — pytest         │   (libghostty    │   `<sessionId>     │
│ — one-shots      │ — CI runners     │   dep)           │   .jsonl`          │
│                  │ — shells (alt)   │   (out-of-tree)  │ — Codex /          │
│                  │                  │   (layer 5)      │   Cursor session   │
│                  │                  │                  │   files (their     │
│                  │                  │                  │   format)          │
└──────────────────┴──────────────────┴──────────────────┴────────────────────┘
```

Tender's job is the centre box. Everything above is a consumer; everything below feeds the stream.

**Agent-native session logs** (rightmost column): Claude Code already maintains an append-only JSONL event log per session at `~/.claude/projects/<cwd>/<sessionId>.jsonl`, with rich structure (uuid, parentUuid, message.content[].type=thinking|tool_use|tool_result|text, etc.). Tender does NOT consume this directly — it's the agent's own private format. But consumers built on top of Tender (the egui block UI, for instance) read it alongside Tender's event stream to get full-fidelity content. Tender's hooks fire faster (real-time signals) and Tender's namespace stream is universal across agents; the agent's session log is the canonical content source for that specific agent.

## What Stays the Same

- **Process sitter identity** at the floor. Tender supervises processes; that's the foundation.
- **Mechanism over policy.** Tender records events; consumers interpret them.
- **All existing CLI verbs** (`start`, `exec`, `wait`, `attach`, `list`, `watch`, `log`, `wrap`, `prune`, `push`, `run`) unchanged.
- **No native LLM protocol support.** Tender does not learn `OpenAiCompatible` exec targets, does not parse JSON-RPC, does not track tokens. The [original decision](decision-process-sitter-not-framework.md) holds for *that specific question*. What changes is that *generic event ingestion* from supervised processes is now in scope.
- **Layer 1–3 discipline.** Anything that wants to interpret blocks semantically (Claude-Code-aware UIs, libghostty parsers, dashboards) is layer 5 and ships separately.
- **Agent CLIs run as their existing binary.** Tender supervises the polished CLI the agent ships (`claude`, `codex`, etc.) rather than reimplementing it. Hooks publish structured events into Tender's stream; the agent's own auth, conversation continuity, retry logic, and UI remain unchanged. Tender adds observability and supervision on top — it does not become a new client. SDK-driven reimplementations are out of scope for first-party agent integration; we wrap, we don't replace.
- **All existing tests** stay green. Additive only.

## What Becomes Possible (Without Tender Shipping It)

Once the event stream is published cleanly:

| Consumer | What they build with Tender's stream |
|----------|--------------------------------------|
| **Warp-style block terminal** | libghostty + sub of `tender watch` = command-block UI on top of Ghostty |
| **Wave-style scriptable shell** | Cross-block introspection, tag filtering, AI sidebar piping — all from `tender list` + `tender block get` |
| **Claude Code session inspector** | Hooks emit per-tool-call events; UI subscribes to render tool timelines |
| **CI dashboard** | Pipeline = DAG of blocks; dashboard subscribes and renders flame graphs |
| **Audit log / compliance** | Every supervised run on prod is a queryable block with provenance |
| **Training-data exporter** | Agent actions = blocks → exportable JSONL for RL/fine-tuning |
| **Crash bundle** | Failing block + parent chain + env → reproducible bundle |

None of these are Tender's responsibility to ship. They are *enabled* by the protocol.

## Boundary

| Tender does | Tender does not |
|-------------|-----------------|
| Supervise processes (layer 1) | Render UIs (layer 5) |
| Manage session lifecycles (layer 2) | Parse OSC 133 from byte streams (a separate crate's job) |
| Compose dependencies (`--after`, `--on-exit`) (layer 3) | Decide what counts as "healthy" or when to retry (layer 4) |
| Accept structured events from supervised processes via `event emit` (layer 3) | Interpret what those events *mean* (e.g., Claude Code semantics — that's consumer territory) |
| Publish a versioned NDJSON event stream | Embed libghostty for VT parsing (separate `tender-shell` crate) |
| Store blocks with content-addressed I/O | Provide a query language over outputs (consumers `grep`/`jq` the stream) |
| Track causality (`parent_block_id`, namespace) | Track tokens, costs, model selection (layer 5 framework concerns) |
| Cross-host execution (`--host` + Win32 Job Objects + SSH) | Manage container/VM lifecycles (boundary metadata describes only — see [boundary-metadata](../active/01_boundary-metadata.md)) |

## How This Composes With Existing Backlog

| Backlog item | Relationship |
|--------------|--------------|
| [boundary-metadata](../active/01_boundary-metadata.md) | Blocks gain a `boundary` field via `LaunchSpec.boundary`. No conflict. |
| [provenance-on-lifecycle-transitions](../completed/2026-04-16-provenance-on-lifecycle-transitions.md) | The `transition_provenance` becomes a first-class field on every emitted lifecycle event. Aligns. |
| [agent-hook-routing](../backlog/agent-hook-routing.md) | The skill should teach hooks → `tender emit` as the primary integration pattern for hook-capable agents. |
| [pty-automation](../backlog/pty-automation.md) | Orthogonal — automation is layer 3 control; events are layer 3 observation. |

No backlog item is contradicted. Several are amplified.

## Naming Conventions

To avoid collision with consumer terminology:

- **Tender's primitive**: "event" (`tender event emit`, `tender watch` already streams events)
- **Block** is reserved for *consumer-side* assembly of events into a presentable record. Tender's event stream IS the input that consumers turn into blocks; Tender does not call its own records "blocks" in its CLI surface or schema.

This keeps Tender's vocabulary (`session`, `event`, `annotation`, `lifecycle`) distinct from Warp's (`block`).

## Roadmap

Ordered by leverage-to-effort. Each is or becomes its own backlog/active item.

1. **Complete in-flight Phase 2A.3 PowerShell exec work.** Foundation. No change in scope.
2. **`--namespace` semantic completion** (`tender start --namespace …`, `tender exec --namespace …`, `tender list --namespace …`, `tender watch --namespace`). Already partly shipped; finish the surface. Namespace is the correlation key for everything downstream.
3. **`tender emit` primitive** — shipped 2026-07-07 (PR #4) as event protocol slice 1; see `completed/2026-07-07-event-emit-primitive.md`. Shipped shape follows [event-protocol.md](event-protocol.md) (`tender emit`, envelope `v:1`, no stdin daemon), not the sketch that stood here.
4. **`tender watch --namespace --json` NDJSON multiplexed event stream.** Documented and stabilised. This is the consumer-facing protocol. Specify event shape and ordering guarantees.
5. **`on_exit` callback delivery** finished (already partly designed — see [slice2-on-exit](../completed/2026-03-28-slice2-on-exit.md) — but lifecycle-hook callbacks emit through the event stream now, not as a separate channel).
6. **Tag v0.3.0, ship Homebrew formula.** Production readiness milestone for downstream consumers.
7. **`tender block get <id>` read API** (optional naming aside: `tender event get` if we keep "block" out of CLI). Lets consumers ask Tender for one record by ID, not just subscribe.
8. **`tender-shell` adapter crate** (separate workspace member, depends on libghostty, layer 5). Parses OSC 133 from PTY output of supervised shells; emits per-command sub-events via the new event protocol. Optional. Only build when there's demand for the Warp-style block-terminal UI.
9. **Warp-style terminal UI on Ghostty** — a separate project entirely. Not Tender's code, not Tender's responsibility. Reference implementation to validate the protocol.

Items 1–6 are clean additive work in Tender itself. Item 7 is small. Item 8 is a new crate; isolates libghostty's footprint. Item 9 is downstream.

## Open Questions

These are decisions to make as the work lands, not before.

1. **Subcommand name.** `tender event emit` vs `tender emit` vs reuse `tender annotate` (which exists but may have different semantics). Decide before implementing.
2. **Schema versioning strategy.** Single integer (`schema_version: 1`), semver string, or content-hash? Recommend integer with explicit migration notes per bump.
3. **Event payload size limit.** Hard cap to prevent supervised processes from flooding the store. Recommend 256 KiB per event with explicit overflow handling.
4. **Cross-namespace causality.** If actor in namespace A spawns block in namespace B, does `parent_block_id` cross? Recommend yes (it's just a UUID), but visibility/auth is a separate layer.
5. **Content-addressable I/O storage.** Stdout/stderr bytes deduplicated by sha256? Recommend yes — replays and repeated outputs share storage.
6. **Redaction at capture.** Outputs may contain secrets. Pre-capture hook for redaction, or trust supervised processes to scrub? Recommend pre-capture hook (registerable per-namespace).

## Why Now

Three factors that didn't hold when the original "no framework" decision was made:

1. **Warp's open-sourcing in April 2026** provides reference code for what a block model looks like — and proves it's a viable abstraction at the ~60k-star scale.
2. **Claude Code's hook system shipped to maturity** with PreToolUse / PostToolUse / Stop / Notification / PreCompact. There is now a *standard mechanism* for processes to publish their internal state without Tender knowing anything tool-specific.
3. **`tender watch --namespace` NDJSON stream** is already partially in flight; making it the universal event protocol is incremental.

Together, these mean the protocol-layer gap can be filled with a small additive primitive — not a rewrite. Tender remains lean. Other projects build on top.

## Reference Picture

> Tender supervises processes (still). It records, parents, and republishes structured events from anywhere they originate (new). Consumers — terminals, dashboards, agents — subscribe to the unified stream and build whatever they need on top.
>
> The protocol IS the product.
