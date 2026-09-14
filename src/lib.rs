//! # Tender — an agent process sitter
//!
//! Tender supervises *runs*, not processes: a command executes under a durable
//! per-session sidecar so its lifecycle, output, and exit survive the CLI
//! invocation that launched it. The `tender` binary is a transactional CLI
//! over that durable session model; this crate is the library it is built
//! from.
//!
//! ## Responsibility split
//!
//! - **CLI (transactional).** Parses arguments, resolves namespaces, writes
//!   control requests, reads persisted state, and exits. It writes lifecycle
//!   state only during [`reconcile`] after the sidecar is gone.
//! - **Sidecar (normal lifecycle authority).** Holds the session lock, spawns
//!   the child, writes run-state transitions, captures output, and classifies
//!   the exit. See [`sidecar`].
//! - **Durable record.** `meta.json` is the current run snapshot, `output.log`
//!   is the append-only child-output record, and the segmented [`events`] log
//!   is lifecycle/provenance history. Views are derived from those authorities;
//!   see [`session`] and [`log`].
//! - **Transport is a wrapper, not a second model.** `--host` forwards an
//!   allowlisted subset of commands to a remote `tender` over SSH; the remote
//!   runs the *same* local lifecycle. See [`ssh`].
//!
//! ## Module map
//!
//! - [`model`] — the domain vocabulary: identifiers ([`model::ids`]), the
//!   persisted [`model::meta::Meta`] record, the run
//!   [`state`](model::state)/[`transition`](model::transition) machine, the
//!   launch [`spec`](model::spec), and recorded [`event`](model::event)s.
//! - [`session`] — the on-disk session directory: what is persisted, durable,
//!   and transient.
//! - [`sidecar`] — the per-session supervisor that owns the child.
//! - [`platform`] — the Unix/Windows process-supervision abstraction
//!   ([`Platform`](platform::Platform)): grouping, containment, tree-kill, and
//!   readiness handshake.
//! - [`events`] / [`log`] — the append-only event log and the views projected
//!   from it.
//! - [`exec_frame`] / [`exec_request`] — the framed `exec` request/response
//!   that rides SSH stdin instead of the remote argv.
//! - [`attach_escape`] — the attach client's `Ctrl-\ d` detach escape.
//! - `attach_socket` (Unix) — private, peer-verified attach sockets under the
//!   state root.
//! - [`pty_input`] — the sidecar's single PTY input writer and its control
//!   queue (claim, takeover, release).
//! - [`recording`] — the exact PTY recording codec (versioned binary segments).
//! - [`ssh`] — the remote transport wrapper and its command allowlist.
//! - [`reconcile`] — reconciling recorded state against observed OS reality.
//!
//! ## Design doctrine & architecture
//!
//! This crate documents *what the types are*. The system-level design — the
//! five-layer stack, the seven themes (one authority per fact, durable truth
//! vs. derived views, control plane vs. work plane, labelled inference, …),
//! and the execution-boundary model — lives in the repository docs:
//!
//! - [Architecture overview](https://github.com/grumpydevorg/agenttender/blob/main/docs/architecture/README.md)
//! - [Design principles](https://github.com/grumpydevorg/agenttender/blob/main/docs/design-principles.md)
//! - [Transport boundaries](https://github.com/grumpydevorg/agenttender/blob/main/docs/architecture/06-transport-boundaries.md)

// The crate doc and module summaries above lean on intra-doc links; a broken one
// is a silent documentation regression. Deny them so a bad link fails the build
// locally too — not only under the `doc` CI job's RUSTDOCFLAGS=-D warnings.
#![deny(rustdoc::broken_intra_doc_links)]

pub mod annotation;
pub mod attach_escape;
pub mod attach_proto;
#[cfg(unix)]
pub mod attach_socket;
pub mod directive;
pub mod events;
pub mod exec_frame;
pub mod exec_request;
pub mod log;
pub mod model;
pub mod platform;
pub mod pty_input;
pub mod ready_file;
pub mod reconcile;
pub mod recording;
pub mod session;
pub mod sidecar;
pub mod ssh;
