---
id: egui-block-terminal
depends_on: []
links:
  - ../specs/tender-as-block-runtime.md
---

# `tender-blocks-egui` — Warp-Style Block Terminal UI (egui + libghostty)

A reference-implementation Rust GUI that consumes Tender's event stream and renders Warp-style command blocks. Built on **egui** for the GUI layer and **libghostty/vt** (separate crate) for the VT/grid layer. Validates the protocol that the [tender-as-block-runtime spec](../specs/tender-as-block-runtime.md) describes.

Lives in a separate workspace member (`tender-blocks-egui/`) — does NOT pull libghostty or egui into core Tender.

> **Event-surface note (2026-07-06):** references below to `tender event
> emit`, `tender watch` over a Unix socket, and "one Tender daemon / local
> socket" predate [event-protocol.md](../specs/event-protocol.md) — no
> daemon or socket exists. The consumption surface is `tender events
> --follow --kind hook.` (+ `--include-logs` for the merged stream) once
> protocol slice 2 lands, or compat `watch` (stdout NDJSON, knowingly)
> before that. Cursors/replay come from `tender events`, not a socket.
> Also per the 2026-07-06 libghostty review: bindings via the community
> `libghostty-vt` crate (crates.io) or a tender-authored -sys crate; store
> PTY dimensions with recorded bytes (replay at original dims, then
> resize); render-state is viewport-only, so the block timeline needs a
> terminal instance per block. The hard `content-addressable-storage`
> dependency is dropped (first slice never uses it).

## Why

Tender's spec positions its event stream as the universal protocol for supervised execution. A reference consumer proves the protocol is sufficient and provides a usable Warp-style UI as a side benefit.

**Why egui specifically**:

- Native Rust, no Electron, no JS runtime
- Already invested-in: existing skill (`egui`), existing reusable widgets (e.g. lib/sst-spectrogram-egui pattern)
- Immediate-mode model fits the "rebuild from event stream every frame" paradigm cleanly
- Cross-platform (macOS / Linux / Windows / WASM) with one codebase
- Composable widget library — block view becomes a reusable component anywhere egui is used

**Why libghostty/vt for VT**:

- Embeddable C/Zig library with Rust bindings via FFI
- Battle-tested ANSI/SGR/OSC/Kitty-graphics parsing
- Used in production by ghostty itself (~55k stars)
- Avoids reimplementing a VT parser

## Architecture — dual-view, three concurrent event sources

The window presents **two synchronized views** of one supervised session:

```
┌─────────────────────────────────────────────────────────────────────┐
│  tender-blocks-egui window                                           │
├────────────────────────────────────┬────────────────────────────────┤
│                                    │                                │
│  CHAT VIEW  (verbatim TUI)         │  BLOCK TIMELINE  (augmented)   │
│                                    │                                │
│  libghostty parses PTY bytes →     │  Each turn / tool call / hook  │
│  egui paints cells                 │  rendered as its own block     │
│                                    │                                │
│  token-by-token, sub-10 ms         │  sub-second block-level lag    │
│  the supervised agent's            │  from canonical event sources  │
│  polished output, unchanged        │                                │
└──────┬─────────────────────────────┴──────┬─────────────────────────┘
       │                                    │
       │ PTY bytes                          │ correlated events
       │                                    │ (sessionId, uuid, tool_use_id)
       │                                    │
       ▼                                    ▼
┌────────────────────────────────────────────────────────────────────┐
│  EVENT MERGE LAYER  (in-process, ~200 lines)                       │
│                                                                    │
│  3 concurrent producers feed one logical event stream:             │
│                                                                    │
│    ① PTY bytes        from tender's supervised process              │
│       → libghostty → cells → chat view                             │
│       → can also be hashed + stored for replay                     │
│                                                                    │
│    ② JSONL tail       ~/.claude/projects/<cwd>/<sessionId>.jsonl    │
│       → structured assistant/user/tool_use/tool_result events      │
│       → CANONICAL content source for the augmented view             │
│       → block-level granularity (sub-second)                       │
│                                                                    │
│    ③ Hook events      via tender event emit                         │
│       → fastest lifecycle signals (sub-50 ms)                       │
│       → PreToolUse → "running" placeholder block                    │
│       → PostToolUse → mark complete with exit / duration            │
│                                                                    │
│  Merged into one block-index keyed by uuid + tool_use_id.          │
└────────────────────────────────────────────────────────────────────┘
       ▲              ▲                ▲
       │              │                │
       │ libghostty   │ inotify        │ unix socket
       │ on PTY       │ kqueue         │ (tender watch)
       │              │ ReadDir...     │
       │              │                │
   ┌───┴──────────────┴────────────────┴───────────────────────────┐
   │ TENDER (supervisor)                                            │
   │   tender start --pty -- claude                                 │
   │   - owns PTY                                                   │
   │   - publishes hook events to the namespace stream              │
   │   - persists supervision lifecycle events                      │
   └────────────────────────────────────────────────────────────────┘
                          ▲
                          │ tender start --pty
                          │
   ┌──────────────────────┴─────────────────────────────────────────┐
   │ CLAUDE CODE  (the supervised agent, unmodified)                 │
   │   - renders polished TUI to its PTY                             │
   │   - writes session JSONL                                        │
   │   - fires hooks for lifecycle events                            │
   └─────────────────────────────────────────────────────────────────┘
```

The UI is a wrapper, not a replacement. Claude Code (or any other agent) runs as its normal binary; its TUI is rendered verbatim in the chat view via libghostty; the block timeline materializes structurally from JSONL + hooks alongside.

## Why three sources, not one

| Source | Latency | Granularity | Carries |
|---|---|---|---|
| PTY bytes (via libghostty) | <10 ms | per-token | exactly what Claude paints, including streamed token chunks |
| JSONL tail | <1 s | per-content-block | canonical structured content (thinking, tool_use, tool_result, attachments) |
| Hook events | 10-50 ms | per-lifecycle-event | fast signals to draw placeholder blocks before content lands |

The chat view needs token-level fidelity → only PTY bytes do that. The block timeline needs structured content (which tool, what input, what output, what exit code) → JSONL is the source of truth. Hooks bridge the latency gap so the timeline shows "Bash running…" within 50 ms of Claude deciding to use a tool, before JSONL finishes writing.

No single channel covers all three needs. The merge layer interleaves them by `sessionId` + `uuid` + `tool_use_id`.

## Synchronization

```
T+0     user types prompt + Enter into chat view (PTY)
T+~ms   chat view shows the typed text echoed by Claude's TUI
T+~s    Claude decides to use Bash; hook PreToolUse fires
T+50ms  block timeline draws placeholder: "🔧 Bash · make build · running…"
T+~s    JSONL writes the assistant turn with the tool_use block
        → block timeline replaces placeholder with canonical record
T+4s    Bash completes; PostToolUse fires
T+4s    block timeline marks block complete: "✓ 4.2s"
T+~s    JSONL writes the user-turn with tool_result block
        → block timeline shows the actual output
```

Both views are watching the same underlying activity; they stay in sync because they share identifiers and timestamps. Slight drift is invisible at human scale.

## Event source variants

Tender wraps the supervised agent. The egui app reads events from whichever sources the agent provides:

| Agent | Chat view source | Timeline source(s) |
|---|---|---|
| **Claude Code** (canonical case) | PTY bytes (TUI) | JSONL tail + hooks |
| **Other agent CLIs with session logs** | PTY bytes (TUI) | their session log + hooks |
| **Generic shell with OSC 133** (future) | PTY bytes | OSC 133 boundaries + hooks |
| **Plain `tender start -- cmd`** (one-off) | PTY bytes | Tender's own lifecycle events |

For agents without a structured session log, the block timeline degrades to hook events + Tender lifecycle events only. Still useful but less rich.

## Out of scope — scripted "stream-json" mode

Claude Code has a `--output-format=stream-json --include-hook-events` mode that emits everything (content + hooks) as a single ordered structured stream on stdout. It is `--print` mode only — replaces the TUI with a programmatic interface.

That use case (fully scripted automation; no human-in-the-loop chat) is a **separate component / future plan**. This plan covers the wrap-the-TUI dual-view design. The two modes share the BlockWidget but have different event-source plumbing, and trying to unify them prematurely complicates both.

## First Slice Goal

A standalone window that:

1. Spawns `tender start <name> --pty -- claude` (session name is positional in the shipped CLI) — or attaches to an existing one
2. Renders Claude Code's TUI in the left pane via libghostty (chat view)
3. Tails `~/.claude/projects/<cwd>/<sessionId>.jsonl` and renders the block timeline in the right pane
4. Subscribes to `tender watch --namespace <name>` for hook events; draws placeholder blocks within 50 ms
5. Merges all three sources into one block-index keyed by `uuid` / `tool_use_id`

Not in first slice: input editor (use Claude's TUI for input), splits, tabs, AI sidebar, agent integrations beyond Claude Code, replay UI, scripted (stream-json) mode.

## Block widget composition

The block widget is the reusable atom. Composition:

```
+-----------------------------------------------------------+
| 📁 ~/Git/repo          🌿 feature/x       exit 0  ⏱ 4.2s   |  ← header
|-----------------------------------------------------------|
| $ make build                                              |  ← prompt
|                                                           |
|   Compiling foo v0.1.0                                    |  ← stdout/stderr region
|   Finished `release` profile [optimized] target(s) ...    |     (cell grid from libghostty)
|                                                           |
|-----------------------------------------------------------|
| 🏷 wip · pr-456                          [↳ replay] [diff] |  ← footer (tags + actions)
+-----------------------------------------------------------+
```

The grid region is what libghostty parses + egui paints. The header and footer come from the block record's structured metadata.

## Layouts (consumer-side, not Tender's concern)

The UI projects Tender's block graph into a visual layout. Multiple projections coexist:

| Projection | What it shows |
|------------|---------------|
| **Per-namespace timeline** (default — Warp-like) | Vertical scroll of blocks in time order within one namespace |
| **Per-tag filter** | Only blocks with a specific tag (e.g. `tag=pr-456`) |
| **Causal tree** | Parent → children expansion; useful for agent sessions |
| **Per-host group** | Blocks grouped by execution host |
| **Failed-only** | All blocks with non-zero exit, across namespaces |

Switching projections does not touch Tender — it's a UI-side reorder of the same underlying event stream.

## Replay & re-render

Because outputs are stored content-addressed (see [content-addressable-storage](content-addressable-storage.md)), the UI can:

- ask Tender for `block.stdout_sha256` content at any time
- re-flow the cell grid at any window size (libghostty re-parses bytes at new dimensions)
- scroll through historical blocks without re-running anything

This is the key advantage of CAS + libghostty over Wave-style "rendered bytes in scrollback": the UI controls presentation, not the renderer that wrote the bytes.

## Composition with existing egui ecosystem

The block widget is reusable in other egui apps:

- a dashboard that embeds a block view next to a chart
- a notebook-style app where cells are blocks
- an agent-session viewer embedded in an IDE
- the existing `sst-spectrogram-egui` pattern: build the block view as a standalone composable, used by multiple consumers

Encourages the same component-library discipline as the spectrogram widget.

## Scope

- new workspace member `tender-blocks-egui/` (separate Cargo manifest)
- depends on: `tender` (for client SDK), `eframe`/`egui`, libghostty bindings (Rust crate exposing the C/Zig ABI)
- runs as standalone GUI binary `tender-blocks` (cross-platform)
- subscribes to one Tender daemon at a time (local socket; remote via SSH-forwarded socket in v2)
- tails the supervised agent's session log file (e.g. Claude Code's `~/.claude/projects/<cwd>/<sessionId>.jsonl`) for structured content events
- renders dual-view layout: chat (libghostty over PTY) + block timeline (merged events)
- reusable `BlockWidget` egui component exported by the crate

## Non-goals

- **Not a terminal emulator.** Does not own a PTY. Does not run shells. Tender does that.
- **Not in core Tender.** Lives in a separate workspace member that *depends on* Tender — never the reverse.
- **No AI integration in v1.** That's a later layer.
- **No theming engine.** Use egui defaults; advanced theming deferred.
- **No mobile / web build in v1** (egui supports WASM, but defer).
- **No native Windows-specific features** beyond what egui provides cross-platform.
- **No SDK-driven agent reimplementation.** Claude Code (and other agent CLIs) run as their normal binary under `tender start --pty`; hooks publish events into Tender's stream; the block UI renders from that stream. Tender + this UI are a wrapper, not a replacement — the agent's polished CLI keeps doing what it does well, and we layer block-structured observability on top.

  Warp's first-party `claude-code-warp` plugin uses the same Claude Code hook protocol (via OSC 777 to `warp://cli-agent`) but currently only for OS notifications and tab status indicators — not per-tool-call block rendering, persistent queryable history, multi-consumer event streams, or cross-host visibility. The differentiation is depth-of-integration on a shared primitive, not exclusive access to it. If Warp extends their plugin to do more, Tender's value shifts toward portability (any UI consumes the stream; not tied to one terminal) and headless / cross-host operation.

## Open questions

1. **libghostty Rust bindings**: do we use an existing crate (e.g. `libghostty-vt`) or build our own thin wrapper? Investigate before starting.
2. **Cell grid → egui paint**: text-only is straightforward; SGR colors map to egui colors; Kitty graphics → egui textures. How much fidelity in v1? Recommend text + SGR colors + OSC 8 hyperlinks; defer graphics.
3. **JSONL discovery**: how does the egui app know which `<sessionId>.jsonl` to tail? Options: (a) Tender records the path when it spawns `claude` (via env vars or process-tree inspection); (b) sniff the writes after launch by watching `~/.claude/projects/<cwd>/` for new files; (c) Claude exposes a deterministic path via env var. Recommend (a) — have Tender record the spawned child's JSONL path as part of its lifecycle event.
4. **Schema drift in Claude's JSONL**: it's an unofficial / private schema. Pin the field names we care about (`uuid`, `parentUuid`, `type`, `message.role`, `message.content[].type`, `tool_use_id`); fail gracefully on unknown variants. Track Claude Code releases that change the schema.
5. **Live tail vs scroll-back**: when subscribing mid-session, do we backfill from the event log or start from "now"? Recommend backfill last N seconds or until first SessionStart event.
6. **Multi-namespace view**: tabs? Single window per namespace? Recommend single window with a namespace picker in v1; tabs later.
7. **Remote namespace subscription**: Tender daemons on remote hosts — connect via SSH-forwarded socket, or via `tender --host` API? Recommend the latter once the API stabilises.
8. **Chat-view input forwarding**: when the user clicks the chat-view pane and types, do keystrokes go through the egui app to Tender's PTY (cleaner; we own the pane) or does the chat pane become a passthrough widget that forwards directly? Recommend passthrough for the typing UX to match Claude's TUI exactly.

## Acceptance criteria

- standalone binary launches `tender start --pty -- claude` (or attaches to an existing session) and shows both views
- **chat view** renders the agent's TUI faithfully from PTY bytes via libghostty: SGR colors, OSC 8 hyperlinks, cursor positioning, token-by-token streaming
- **block timeline** renders one block per turn / tool call, populated from JSONL tail + hooks
- hook PreToolUse → placeholder block appears in timeline within 100 ms
- JSONL update → block's canonical record replaces placeholder within 1 s of write
- hook PostToolUse → block marked complete with exit + duration within 100 ms
- correlation works: same `tool_use_id` from hook and JSONL maps to one block (no duplicates)
- handles agent session restart / `/rewind` (branched conversations via multiple JSONL tips)
- handles 10k+ blocks in timeline without UI jank (virtualized scrolling)
- block widget is a reusable egui component (used both in `tender-blocks` and as a library)
- exits cleanly on Tender daemon disconnect, JSONL stops growing, or agent exit; reconnects on daemon return
- documented; example screenshots in repo showing both views in sync

## Depends On

- `event-emit-primitive` — **shipped** (2026-07-07); the structured events that give blocks their sub-structure exist, and `tender events --follow`/`--include-logs` (shipped 2026-07-07) is the consumption surface. Tender-side deps are satisfied.
- `content-addressable-storage` — **not required.** The first slice never uses it (see the banner above); size-adaptive re-render off stable blob refs is a later enhancement, not a gate. Residual risk is external (libghostty's unstable C API), not a tender blocker.
