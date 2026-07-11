# Sidecar Control Protocol — Design Spec

Target architecture for portable, typed control IPC between the Tender CLI and the per-session sidecar. This spec describes a future control plane shape. It does not commit to a timeline and is explicitly not a prerequisite for any current backlog item including PTY automation.

## Status

Design spec only. Not scheduled. Not blocking.

## Migration trigger

Build the control protocol when either:

- a feature needs portable correlated request/response over a boundary that files cannot serve cleanly, or
- a required remote control flow cannot be represented cleanly as a one-shot typed Tender CLI frame over SSH stdio.

Do not build it because the file-based IPC pattern count reaches some threshold. The decision is about boundary-crossing demand, not pattern proliferation.

## The problem

Tender's current IPC is file-based and local:

- `kill_request` — atomic file write, sidecar polls, consumes
- PTY lease IPC — per-request files under `lease/requests/` and `lease/responses/`
- push authorization — framed `PushHeader` on the stdin FIFO
- push rejection — out-of-band `push_rejects/<request_id>.json`
- attach — Unix domain socket with framed messages

This works well on a single host with a shared filesystem. It is inspectable, durable, restart-tolerant, and requires no connection state. Those are real advantages for a process sitter.

It does not generalize cleanly across:

- container boundaries without shared mounts
- Windows named pipes vs Unix FIFOs
- SSH transport (current `--host` cannot carry `exec` or other coordinated commands)
- features that need reliable correlated request/response with typed errors

## Target architecture: three layers

The control protocol does not replace Tender's existing strengths. It adds a middle layer.

### Layer 1 — Durable truth (unchanged)

`meta.json` and `output.log` remain on disk. They are not IPC; they are crash recovery, observability, reconciliation substrate, and offline inspection. No protocol replaces them.

### Layer 2 — Control IPC (new)

A framed request/response/event protocol over a duplex stream. One protocol, multiple carriers:

- Unix: Unix domain socket (default)
- Windows: named pipe
- SSH: stdio bridge to the same API
- Container boundary: mounted socket or sidecar-local CLI bridge

### Layer 3 — Work IPC (unchanged)

Raw byte streams for stdin payloads, PTY terminal traffic, and the attach relay. These stay as dedicated channels. Do not multiplex control and work traffic onto one stream.

## Control envelope

```rust
#[derive(Debug, Serialize, Deserialize)]
pub enum ControlMsg {
    Request(ControlRequest),
    Response(ControlResponse),
    Event(ControlEvent),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControlRequest {
    pub request_id: Uuid,
    pub method: Method,
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControlResponse {
    pub request_id: Uuid,
    pub result: ControlResult,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ControlResult {
    Ok { data: serde_json::Value },
    Err { code: String, message: String, details: Option<serde_json::Value> },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControlEvent {
    pub event_type: String,
    pub data: serde_json::Value,
}
```

### Framing

Length-prefixed JSON over the duplex stream. Not newline-delimited — length-prefix is unambiguous over arbitrary transports (sockets, pipes, stdio). Four bytes big-endian length, then that many bytes of UTF-8 JSON.

### Transport trait

```rust
pub trait ControlTransport: Send {
    fn send(&mut self, msg: &ControlMsg) -> Result<()>;
    fn recv(&mut self) -> Result<ControlMsg>;
}
```

Backed by `UnixStream`, Windows named pipe, or an SSH stdio bridge. The trait is the seam; transport implementations are thin.

## Candidate methods

These are the operations that would migrate from file-based IPC to control RPC if and when the protocol is built.

| Method | Current IPC | Why it's a candidate |
|--------|-------------|---------------------|
| `request_kill` | `kill_request` file | Correlated request/response; currently fire-and-poll |
| `acquire_lease` | `lease/requests/<id>.json` | Correlated; needs typed error on denial |
| `refresh_lease` | Same | Same |
| `release_lease` | Same | Same |
| `authorize_push` | Framed `PushHeader` on FIFO | Currently one-way; reject is out-of-band file |
| `watch_subscribe` | CLI polls `output.log` | Event stream is the natural fit for control RPC |
| `get_status` | CLI reads `meta.json` | Could be live rather than file-poll |
| `exec_begin` | CLI-side FIFO + lock + log scan | **The big one**: would make exec remotable |

### What stays out

| Primitive | Reason |
|-----------|--------|
| `meta.json` | Durable truth, not live IPC. Must survive sidecar death. |
| `output.log` | Append-only audit trail. Must survive sidecar death. |
| PTY raw bytes | Work plane. Dedicated socket with its own framing. |
| stdin payloads | Work plane. Raw bytes after auth header. |
| Attach relay | Work plane. Already has its own socket + message types. |

## What this is not

- **Not a second lifecycle model.** The sidecar still owns lifecycle truth; the protocol is how clients talk to it, not a competing authority.
- **Not a container manager.** Boundary metadata (see `completed/2026-07-10-boundary-metadata.md`) describes where sessions run; the control protocol does not manage those environments.
- **Not a durability replacement.** `meta.json` and `output.log` survive sidecar death. RPC connections do not. The protocol augments file-based truth with live control; it does not replace it.
- **Not HTTP or gRPC.** Too much ceremony for sidecar-local control. Length-prefixed JSON over a duplex stream is sufficient.
- **Not a multiplexed mega-stream.** Control, work, and terminal traffic stay on separate channels. Mixing semantics violates the control/work plane split (Theme 5).

## Relationship to current plans

### PTY automation (`backlog/pty-automation.md`)

PTY automation is designed against file-based lease IPC and does not depend on this protocol. Note (2026-07-09): PTY automation is now **deferred hardening** — built only if real PTY-input contention actually shows up (see `backlog/pty-automation.md`). If it is ever revived, the preserved file-based lease design stands on its own; and if this control protocol already exists by then, lease operations are natural migration candidates onto it. Either way PTY automation neither gates nor is gated by this protocol.

### Provenance (`completed/2026-04-16-provenance-on-lifecycle-transitions.md`)

Already shipped. The provenance model (Direct vs Inferred) applies equally to file-based and RPC-based writes. No interaction.

### Boundary metadata (`completed/2026-07-10-boundary-metadata.md`)

Describes where sessions run. The control protocol describes how clients talk to the sidecar. Orthogonal.

### Exec annotation ergonomics (`completed/2026-07-09-exec-annotation-ergonomics.md`)

Annotation noise and breadcrumbs are log-side concerns. If `exec_begin` migrates to control RPC, the annotation model would still write to `output.log` via the sidecar, not via the protocol.

## Design constraints

- The sidecar is currently sync-threaded, not async. The control socket listener would be another thread in the sidecar's existing poll loop, not a Tokio runtime. Keep this in mind for the transport implementation.
- The protocol must handle sidecar restart gracefully. Clients connecting to a dead socket should get a clear transport error, not a hang. The file-based `meta.json` remains the crash-recovery path.
- Multiple concurrent clients (e.g., two agents calling `get_status`) must be supported. The listener accepts multiple connections; each request is independently correlated by `request_id`.

## When to revisit

Re-evaluate this spec when:

- a required remote flow needs long-lived or correlated sidecar interaction that the typed remote CLI frame cannot serve
- a new feature proposal requires correlated request/response that file IPC cannot cleanly serve
- the file-based IPC surface has grown beyond "files + framed FIFO" into a pattern that resists local reasoning

Until then, this spec is a north star, not a plan.
