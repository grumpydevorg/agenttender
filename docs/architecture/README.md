# Tendr Architecture

This directory maps the current Tendr system as implemented on `main`.

It is architecture-level documentation: process boundaries, session storage, lifecycle state, PTY control, key flows, and transport boundaries. It intentionally does not duplicate every struct field or every helper function.

For the review doctrine that should shape new features, see [../design-principles.md](../design-principles.md).

Read these in order:

1. [01-system-context.md](01-system-context.md) — what the running system is, who talks to it, and where responsibility sits
2. [02-session-storage.md](02-session-storage.md) — what Tendr persists on disk, what is durable, and what is transient
3. [03-run-lifecycle.md](03-run-lifecycle.md) — the run state machine and who is allowed to write it
4. [04-pty-lane.md](04-pty-lane.md) — the PTY execution lane and current human/agent control model
5. [05-key-flows.md](05-key-flows.md) — the load-bearing sequences: `start`, `exec`, `kill`, and `attach`
6. [06-transport-boundaries.md](06-transport-boundaries.md) — the concrete IPC and transport surfaces: pipe, file, socket, lock, and SSH

Scope notes:

- These diagrams describe the current codebase, not the full roadmap.
- The planned remote-first PTY control, recording, and external screen extension are tracked in [cloud PTY control and replay](../plans/active/00_cloud-pty-control.md); the older [lease design](../plans/backlog/pty-automation.md) is preserved as history.
- Remote execution is transport-only: the same local lifecycle model is invoked over SSH for the currently allowlisted commands.
