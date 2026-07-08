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
section to `.agents/skills/using-tender/SKILL.md`: tender for
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

First slice: validate this end-to-end with a real Claude Code session and
document the pattern here and in the skill.

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

### 5. Native rendered-state reads in tender (strategic, deliberate scope call)

The sharpest confirmed gap: tender waits only on exit; boo waits on screen
content/quiescence. Long term, tender's PTY lane wants `peek`/`wait --text`
natively. The pieces exist (maintained `libghostty-vt` crate on crates.io —
Uzaaft/libghostty-rs; boo as a ~1,200-LOC-of-glue reference; the sidecar
already holds the PTY master). But this reverses `pty-automation.md`'s
explicit "What NOT to Build" list and touches the keep-libghostty-out-of-core
layering — decide it together with the egui/block-runtime work, not as a
bolt-on. If built: VT state lives in the sidecar, exposed via the planned
sidecar control protocol (not a second attach client — attach is
single-client and steals).

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

- using-tender skill has a tender-vs-boo routing section.
- The `BOO_FOREGROUND=1` composition pattern is validated against a real TUI
  session and documented with its limitations.
- No tender code changes required.
