---
id: ecosystem-landscape
depends_on: []
links:
  - ./tender-as-block-runtime.md
  - ./tender-agent-process-sitter.md
  - ../backlog/boo-integration.md
  - ../backlog/egui-block-terminal.md
---

# Ecosystem Landscape & Lanes — where this all sits

Written 2026-07-06 after three deep reviews (the block-runtime doc wave,
libghostty, and coder/boo). This spec answers two questions: **where tender
sits** in the emerging agent-terminal ecosystem, and **which parts of our
own roadmap are orthogonal** to the core and should not block it (or be
blocked by it).

## The map

| Concern | Who does it today | Notes |
|---|---|---|
| Process supervision, durable transcripts, structured exec results, deps/hooks, remote, Windows | **tender** (shipped) | files-as-truth, sidecar-per-session, no daemon |
| Live TUI screen state for agents (send/peek/wait, rendered screen) | **boo** (coder/boo, shipped) | in-memory only, no exit codes, no events, POSIX-only — see [boo-integration](../backlog/boo-integration.md) |
| Embeddable VT/grid engine | **libghostty-vt** (ghostty-org/ghostty) | real and good; main-branch-only, unstable C API; third-party Rust crate exists (`libghostty-vt` on crates.io) |
| Block-style terminal UX | Warp (closed), planned tender-blocks-egui | consumer of tender's protocol, never core |
| Structured event protocol between supervision and presentation | **nobody yet** | this is tender's block-runtime ambition — still the real gap |

boo is the closest neighbour and confirms rather than refutes the gap: it is
well-engineered screen-state-as-truth with **no** structured events, exit
codes, durability, or remote. The tools bisect the space today. The
positioning risk is trajectory, not overlap: Coder has distribution and
motive to grow toward structured results.

## State of our own docs (as of 2026-07-06)

The 2026-05 block-runtime doc wave (tender-as-block-runtime,
persistence-architecture, event-emit-primitive, CAS, analytics, egui,
completer, Hermes bridge, agent skill) was deep-reviewed with adversarial
verification: 93 confirmed findings. The three load-bearing problems, which
gate any implementation work against those docs:

1. **One event schema, one owner.** Three docs define mutually incompatible
   event envelopes (field names, ts type, oversize-payload behavior,
   block_id semantics). The emit plan should own the schema; everything else
   conforms.
2. **The daemon is an undecided decision.** Five docs silently assume a
   resident per-host daemon; the accepted process-sitter spec says "not a
   daemon"; a plain O_APPEND JSONL write satisfies the emit plan's own
   acceptance criteria. Decide explicitly or rescope emit as daemonless.
   *(Resolved 2026-07-06: daemonless, via a judged three-design bake-off —
   see [event-protocol.md](./event-protocol.md), now the schema owner,
   which also settles problem 1 for events.)*
3. **Stale-reality sweep.** Multiple docs describe shipped work as pending
   (PowerShell side-channel, --namespace, on_exit, provenance) and ignore
   shipped mechanisms (`tender wrap`, the using-tender skill). Every
   "current behavior" claim needs re-verifying against HEAD.

libghostty verdict (for the egui/tender-shell satellites): the library is
real and better than our docs assumed (full terminal-state + render API with
dirty tracking; streaming native; serious packaging), but: no tagged release
ships the needed headers (pin main), API explicitly unstable
(single-maintainer), OSC 133 **exit codes are unreachable via the C ABI**
(tender-shell must parse OSC 133 itself — as the spec already says), replay
of stored PTY bytes at a different width corrupts geometry-dependent output
(store PTY dims with recorded bytes; replay at original dims, then resize),
and the render-state API is viewport-only (a multi-block timeline needs a
terminal instance per block).

## The lanes (hive-off)

Work is split into four lanes. **A lane must not block, or be blocked by,
another lane** except where a dependency is stated explicitly.

### Lane A — core sitter (this repo, the active queue)

The process sitter tender already is. Near-term queue:
remote-exec-host-parity; a **daemonless** event-emit primitive (rescoped per
finding 2 above); doc-index hygiene. Cheap boo-inspired wins live here too:
`unread`/bell turn signals in status/watch, detached terminal-query
answering for PTY sessions.

### Lane B — satellites (orthogonal; separate projects or workspace members)

Consumers of tender's surface. None of these may gate Lane A, and Lane A
must not grow features that exist only for them:
`egui-block-terminal`, the `tender-shell` OSC-133 adapter,
`tender-completer`, and `agent-hook-routing`. Their plan docs stay in backlog/
but carry this lane label. The egui plan's hard dependency on CAS is dropped
(its first slice never uses it — confirmed finding).

_(Groomed 2026-07-09: `hermes-block-runtime-integration` and
`skill-agent-block-runtime` were collapsed into the single small
`agent-hook-routing` item — the skill core already shipped as `using-tender`,
and the Hermes doc was pre-`event-protocol.md` glue. The umbrella "teach agents
the Tender CLI" epic is retired.)_

### Lane C — storage architecture (decision-gated)

`persistence-architecture`, `content-addressable-storage`,
`event-log-analytics`. All three were written against the now-rejected
daemon assumption (see [event-protocol.md](./event-protocol.md)) and
disagreed on schema/paths. **The gate is now lifted:** the Lane A event
schema shipped (event-protocol.md, the daemonless envelope) and the daemon
question is decided. Accordingly (2026-07-09) `event-log-analytics` was
rewritten to the shipped v1 envelope, promoted, then shipped as
[`tender query`](../completed/2026-07-09-event-log-analytics-v1.md) — the
least-speculative Lane C item, DuckDB over JSONL.
`content-addressable-storage` stays deferred: its blob
primitive was already absorbed into event-protocol (`events/blobs/<sha256>`),
and the remaining provenance/bundle ambition is consumer-gated.

### Lane D — ecosystem interop (external-facing, opportunistic)

[boo-integration](../backlog/boo-integration.md) (skill routing →
supervised composition → exec target), libghostty dependency posture
(pin a commit; prefer the community Rust crate; upstream OSC-133 accessors
if the relationship allows). Lane D items are docs-and-glue: they must not
add dependencies to Lane A.

## Non-goals

- Competing with boo on rendered-screen UX for its own sake. If Lane B's
  egui work makes native peek/wait cheap, that decision is taken there
  (see boo-integration path 5), not smuggled into Lane A.
- Adopting libghostty anywhere in core tender. It remains a Lane B
  dependency only.
