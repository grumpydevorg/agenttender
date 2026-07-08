# Tender Plans

Spec: [tender-agent-process-sitter.md](specs/tender-agent-process-sitter.md)

Convention: see [CONVENTIONS.md](CONVENTIONS.md)

## active/ — Current Work

Filename prefix sets priority. `ls active/` is the ordered queue.

_Empty — no active plan. Promote a backlog item (give it a numbered
prefix) to queue the next unit of work._

## backlog/ — Future Work

| ID | File | Depends On |
|----|------|------------|
| fleet-migration | `fleet-migration.md` | remote-ssh-transport |
| exec-annotation-ergonomics | `exec-annotation-ergonomics.md` | — |
| pty-automation | `pty-automation.md` | pty-session-mode |
| powershell-exec-framing | `powershell-exec-framing.md` | — |
| hermes-block-runtime-integration | `hermes-block-runtime-integration.md` | event-emit-primitive |
| boo-integration | `boo-integration.md` | — |
| boundary-metadata | `boundary-metadata.md` | — |
| content-addressable-storage | `content-addressable-storage.md` | event-emit-primitive |
| egui-block-terminal | `egui-block-terminal.md` | event-emit-primitive, content-addressable-storage |
| tender-completer | `tender-completer.md` | event-emit-primitive |
| event-log-analytics | `event-log-analytics.md` | event-emit-primitive |
| skill-agent-block-runtime | `skill-agent-block-runtime.md` | all other backlog items |

## completed/

40 completed plans. See `completed/` directory (`ls` is the source of truth for the count).

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
| `event-protocol.md` | **Schema owner** for the structured event stream: daemonless files-first envelope, ordering contract, cursors, watch/wrap migration |
