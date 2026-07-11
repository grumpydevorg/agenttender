# Tender Plans

> **Internal planning archive.** This is the working plan/spec ledger — active
> queue, backlog, completed history, and long-lived design specs. For a short
> public view of direction see [../ROADMAP.md](../ROADMAP.md); for what Tender is,
> start at the [project README](../../README.md).

Spec: [tender-agent-process-sitter.md](specs/tender-agent-process-sitter.md)

Convention: see [CONVENTIONS.md](CONVENTIONS.md)

## active/ — Current Work

Filename prefix sets priority. `ls active/` is the ordered queue. All backlog
`depends_on` prereqs (event-emit-primitive, remote-ssh-transport,
pty-session-mode) have shipped, so nothing is dependency-blocked.

| ID | File | Depends On |
|----|------|------------|
| remote-frame-transport | `00_remote-frame-transport.md` | — |

## backlog/ — Future Work

Groomed 2026-07-09 (see git history). `depends_on` gates are all satisfied;
the live distinction is keep-ready vs deferred-until-a-consumer.

| ID | File | Lane / status |
|----|------|---------------|
| agent-hook-routing | `agent-hook-routing.md` | Lane B/D — small docs/glue; ready (replaces the cut skill-agent + hermes cards) |
| boo-integration | `boo-integration.md` | Lane D — first slice ready; strategic path-5 deferred |
| content-addressable-storage | `content-addressable-storage.md` | Lane C — deferred; blob primitive already absorbed into event-protocol, rest is consumer-gated |
| pty-automation | `pty-automation.md` | Lane A — deferred hardening (PTY input-lease); gated on real contention. Screen automation is Boo's, not this. |

## completed/

44 completed plans. See `completed/` directory (`ls` is the source of truth for the count).

## specs/

Long-lived design documents (not queue items).

| File | Description |
|------|-------------|
| `tender-agent-process-sitter.md` | Full design spec |
| `tender-as-block-runtime.md` | Positioning: Tender as universal block runtime / event protocol layer |
| `persistence-architecture.md` | Storage layering: event log (source of truth) + in-memory index + blob store. No transactional DB. |
| `decision-process-sitter-not-framework.md` | Decision: no native LLM protocol support (extended by `tender-as-block-runtime.md`) |
| `sidecar-control-protocol.md` | Target architecture: portable sidecar control RPC (not scheduled) |
| `ecosystem-landscape.md` | Where tender sits vs boo/libghostty/Warp + the four work lanes (core / satellites / storage / interop) |
| `windows-parity.md` | Full Windows-parity roadmap (observable-contract parity): the 6-phase plan (CI gate → typed frame → lifecycle hardening → ConPTY/attach → PowerShell), gap inventory + final qualification matrix |
| `event-protocol.md` | **Schema owner** for the structured event stream: daemonless files-first envelope, ordering contract, cursors, watch/wrap migration |
