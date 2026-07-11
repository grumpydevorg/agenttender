//! Domain model — the durable vocabulary every layer agrees on.
//!
//! *One authority per fact:* these types are the shared language of the CLI,
//! the sidecar, and the event log. Identifiers ([`ids`]), the persisted
//! session record ([`meta`]), the run [`state`] machine and its
//! [`transition`]s, the launch [`spec`], recorded [`event`]s, PTY control
//! ([`pty`]), dependency-failure ([`dep_fail`]) and [`boundary`]/[`provenance`]
//! metadata.

pub mod boundary;
pub mod dep_fail;
pub mod event;
pub mod ids;
pub mod meta;
pub mod provenance;
pub mod pty;
pub mod spec;
pub mod state;
pub mod transition;
