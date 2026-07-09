---
id: boo-integration
depends_on: []
links:
  - ../specs/ecosystem-landscape.md
  - ../specs/tender-as-block-runtime.md
  - ./pty-automation.md
  - ./egui-block-terminal.md
  - ../completed/2026-07-08-remote-exec-host-parity.md
---

# boo Integration — compose with, learn from, route around

[coder/boo](https://github.com/coder/boo) is a GNU-screen-style terminal
multiplexer built on libghostty-vt, by Coder Technologies (agent-infrastructure
company). Reviewed in depth 2026-07-06 at v0.6.4 (HEAD 39245a7); facts below
are source-verified at that version.

## What boo is

- ~11.2k LOC Zig. One forked daemon per session (no host daemon), each owning
  one PTY child + one in-memory ghostty-vt `Terminal` (512 KiB scrollback).
  Unix socket per session; discovery is socket-file globbing.
- Consumes ghostty as a **Zig module** pinned to one commit (never bumped),
  not the C ABI.
- Agent-facing surface: `send --text/--key/--stdin`, `peek [--scrollback]
  --json` (the *rendered* screen from VT state — ordered, redrawn,
  cursor-positioned), `wait --text` (screen substring) / `wait --idle` (2 s
  output quiescence), `ls --json` with `unread` and `bell_idle_ms` ("your
  turn") signals. All TTY-less, documented exit codes, dedicated
  `boo help automation` page.
- Notable engineering: answers terminal queries (DA/DSR/XTWINOPS/OSC 11) from
  emulated state **while detached** so TUIs never hang unattended;
  non-blocking per-client output queues with an 8 MiB drop cap; process-tree
  teardown with pid-reuse guards (`reap.zig`).
- 26 days old at review, 96% single-author (Coder's CTO, substantially
  agent-written commits), MIT, POSIX-only (no Windows), 0.x with three
  breaking CLI changes in its first week.

## What boo lacks (tender's moat — keep it loud)

- **Zero durability.** All state is daemon heap. Crash/reboot loses the
  screen, the scrollback, and the record that the session existed.
- **No exit codes anywhere.** SIGCHLD status is discarded; `wait` has no
  exit concept. An agent cannot learn whether `boo new build -d -- make`
  succeeded.
- No event stream (wait = 50 ms client polling), no hooks, no dependencies,
  no timeouts, no remote/multi-host, no Windows, one attached client
  (attach steals), `peek --scrollback` silently truncates at the 1 MiB
  frame cap.

The tools bisect the space: boo = "agent drives an interactive TUI and reads
the screen"; tender = "agent supervises processes and gets structured
results, transcripts, and events". Every integration path below exploits
that split.

## Integration paths, ranked

### 1. Skill routing (do first, docs only)

Agents with both tools installed get steered to boo for interactive work by
its automation help; the using-tender skill has no boundary guidance. Add a
section to `.claude/skills/using-tender/SKILL.md` (the maintained copy): tender for
run-to-completion, structured results, durable logs, deps/hooks, remote,
Windows; boo for driving/reading live TUIs; the composition pattern below
for both at once.

### 2. Composition: tender supervises boo daemons (no code)

`BOO_FOREGROUND=1` skips boo's daemon fork (`src/main.zig:338-350`), so the
daemon can run as tender's supervised child:

```sh
tender start claude-tui --namespace agents -- \
  env BOO_FOREGROUND=1 boo new claude -- claude
```

tender contributes durable lifecycle (meta.json survives reboot as a record,
exit capture of the daemon, `--replace`, `--timeout`, `on_exit`, `watch`
lifecycle events); agents drive the TUI through boo's socket. Known
limitation: tender's `output.log` records boo's lifecycle, not PTY bytes
(those flow daemon→socket-clients only).

Ownership split — the clean boundary that makes the stack work:

| Tender owns (durable supervisor, underneath) | Boo owns (screen authority, on top) |
|---|---|
| process lifecycle, restart / `--replace` / kill / `wait` | rendered terminal state (VT / grid) |
| exit status, durable `meta.json` + `output.log` | `send` — keystroke injection |
| structured events (`run.*`, `exec.*`, `hook.*`, `callback.finished`) | `peek` — read the rendered screen |
| `--host` remote transport, deps, `on_exit` | `wait --text` / `wait --idle` — screen quiescence |

Observe the session's lifecycle durably through tender (the supervised session
is `agents/claude-tui`); drive/read the TUI through boo (the boo session is
`claude`):

```sh
tender status claude-tui --namespace agents
tender wait   claude-tui --namespace agents
tender events --session agents/claude-tui --follow

boo send claude --text "run tests"
boo wait claude --text "Done"
boo peek claude --json
```

Rendered-screen reads stay boo's, never tender core (see path 5). Tender keeps
the process alive and accountable; boo makes the terminal legible and
controllable.

**First slice — documented, not yet validated:** the composition pattern and
the ownership boundary are now documented (here + the `using-tender` skill).
Live validation — running a real Claude Code TUI under `tender start`
end-to-end against a **pinned** Boo build — remains open; Boo is not
installed/pinned locally.

### 3. A `boo` exec target in tender

Fits the existing exec-target pattern (posix-shell / powershell /
python-repl / duckdb): frame the command via `boo send --stdin`, await via
`boo wait --text <token>`, harvest via `boo peek --scrollback`. Gives
structured `{stdout, exit_code}` results **on top of** boo sessions — the
thing boo cannot do. Design around: the 1 MiB peek cap and 512 KiB
scrollback bound harvestable output, so large results need a side-channel
file (same trick as the shipped PowerShell side-channel).

### 4. tender as boo's remote transport

boo has no remote story. Remote exec parity shipped 2026-07-08
(`completed/2026-07-08-remote-exec-host-parity`), so
`tender --host <h> exec -- boo peek … --json` already gives quoting-safe
structured remote screen reads. Tender's remote lane is a distribution
advantage over boo rather than a parallel effort.

### 5. Native rendered-state reads in tender — rejected for core

The sharpest confirmed gap: tender waits only on exit; boo waits on screen
content/quiescence. It is tempting to give tender's PTY lane `peek`/`wait --text`
natively, and the pieces exist (maintained `libghostty-vt` crate on crates.io —
Uzaaft/libghostty-rs; boo as a ~1,200-LOC-of-glue reference; the sidecar already
holds the PTY master). **The decision (2026-07-09) is not to build this in tender
core.** Rendered-screen reads are Boo's domain — Boo is the screen authority;
tender supervises the process and owns the durable record, and keeps no terminal
renderer (no libghostty) in core (see `pty-automation.md`'s "What NOT to Build"
and the ecosystem-landscape non-goals). If a native screen layer is ever wanted
it is a separate, deliberately-built satellite/UI decision — taken with the
egui/block-runtime work, never smuggled into core as a bolt-on. Until then, path 4
above (`tender --host <h> exec -- boo peek … --json`) already delivers structured
screen reads by composition, which is the whole point of the stack.

## Ideas to steal regardless of integration

- `unread` + `bell_idle_ms` turn signals: zero-tool-cooperation
  turn-completion detection; fits tender's `status`/`watch` model.
- Detached query answering (DA/DSR/OSC 11) for tender's PTY sessions —
  prevents unattended TUI hangs on its own merits.
- Non-blocking per-connection output queue with a drop cap (fixed a real
  daemon-freeze deadlock in boo).
- Pid-reuse-guarded process-tree teardown.

## Risks

- boo is 0.x and churns its CLI; anything scripted against it must pin a
  version.
- Coder has distribution and a strategic reason (Coder Agents) to keep
  investing; structured results/events are an obvious next step for them.
  Positioning docs should name boo explicitly rather than argue against an
  anonymous field — see [ecosystem-landscape](../specs/ecosystem-landscape.md).

## Acceptance criteria (first slice = paths 1 + 2)

- [x] **Done** — using-tender skill has a tender-vs-boo routing section.
- [x] **Done** — no tender code changes required.
- [ ] **Open** — the `BOO_FOREGROUND=1` composition pattern is *documented* with
  its limitations, but **live validation against a pinned Boo build** (a real
  TUI session under `tender start`) is not done: Boo is not installed/pinned
  locally. This is what keeps the first slice incomplete.
