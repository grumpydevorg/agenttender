//! `tendr emit` — the write surface of the event protocol (spec §6, §7).
//!
//! Exit codes are granular so agents can branch on integers:
//! 0 ok · 2 usage · 3 no session context · 5 session not found ·
//! 6 invalid kind/source (reserved prefix). `--best-effort` maps every
//! failure to 0 — hooks must never fail their host tool.

use std::io::Read;
use std::path::PathBuf;

use tendr::events::{self, EventDraft, EventWriter};
use tendr::model::event::{Kind, Uuid7};
use tendr::model::ids::{Namespace, RunId, SessionName, Source};
use tendr::session::{self, SessionError, SessionRoot};

pub struct EmitOptions {
    pub kind: String,
    pub data: Option<String>,
    pub data_file: Option<PathBuf>,
    pub data_stdin: bool,
    pub source: Option<String>,
    pub session: Option<String>,
    pub parent: Option<String>,
    pub durable: bool,
    pub best_effort: bool,
}

struct EmitFailure {
    code: i32,
    message: String,
}

fn fail(code: i32, message: impl Into<String>) -> EmitFailure {
    EmitFailure {
        code,
        message: message.into(),
    }
}

pub fn cmd_emit(opts: EmitOptions) -> anyhow::Result<()> {
    match emit_inner(&opts) {
        Ok(()) => Ok(()),
        Err(failure) => {
            eprintln!("tendr emit: {}", failure.message);
            if opts.best_effort {
                Ok(())
            } else {
                std::process::exit(failure.code)
            }
        }
    }
}

fn emit_inner(opts: &EmitOptions) -> Result<(), EmitFailure> {
    // 6: kind/source are user-supplied here — reserved prefixes rejected.
    let kind = Kind::new_user(&opts.kind).map_err(|e| fail(6, format!("invalid kind: {e}")))?;
    let source = match &opts.source {
        Some(s) => Source::new(s).map_err(|e| fail(6, format!("invalid source: {e}")))?,
        None => Source::new("user.emit").expect("user.emit is grammatical"),
    };

    // 2: usage.
    let data = read_data(opts)?;
    let parent_id = opts
        .parent
        .as_deref()
        .map(|p| {
            p.parse::<Uuid7>()
                .map_err(|e| fail(2, format!("invalid --parent: {e}")))
        })
        .transpose()?;

    // Ambient causality — one chaining rule (spec §2, §6): block_id from
    // TENDR_BLOCK_ID; parent_id from --parent > TENDR_PARENT_EVENT_ID >
    // TENDR_BLOCK_ID. Ambient env never hard-fails: malformed values warn
    // and are ignored.
    let block_id = events::env_uuid7("TENDR_BLOCK_ID");
    let parent_id = parent_id
        .or_else(|| events::env_uuid7("TENDR_PARENT_EVENT_ID"))
        .or(block_id);

    // 3: session context from --session or the supervised-run environment.
    let (namespace, session_name, from_env) = resolve_context(opts)?;

    // Identity from the environment when supervised. TENDR_RUN_ID may name
    // a prior generation after --replace — correct, not an error (spec §1).
    let env_run_id = std::env::var("TENDR_RUN_ID")
        .ok()
        .and_then(|s| serde_json::from_value::<RunId>(serde_json::Value::String(s)).ok());
    let env_generation = std::env::var("TENDR_GENERATION")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());

    let root = SessionRoot::default_path().map_err(|e| fail(1, e.to_string()))?;
    let session_dir = match session::open(&root, &namespace, &session_name) {
        Ok(dir) => dir,
        Err(SessionError::Corrupt { session, reason }) => {
            return Err(fail(5, format!("session {session} is corrupt: {reason}")));
        }
        Err(e) => return Err(fail(1, e.to_string())),
    };

    let Some(dir) = session_dir else {
        // Orphan emitter (spec §7): a pruned/replaced session known from the
        // environment still has a fully-addressed event — preserve it.
        // Nested rather than a let-chain, to hold the advertised 1.85 MSRV
        // (let-chains are stable from 1.88).
        if from_env {
            if let Some(run_id) = env_run_id {
                let draft = EventDraft {
                    id: None,
                    kind,
                    namespace,
                    session: session_name,
                    run_id,
                    generation: env_generation,
                    source,
                    block_id,
                    parent_id,
                    data,
                    preview: None,
                };
                let event = events::stamp_orphan_event(draft);
                let tendr_root = root
                    .path()
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .ok_or_else(|| fail(1, "session root has no parent"))?;
                events::append_lost_found(&tendr_root, &event)
                    .map_err(|e| fail(1, format!("lost+found append failed: {e}")))?;
                eprintln!("tendr emit: session dir gone; event preserved in lost+found");
                return Ok(());
            }
        }
        return Err(fail(
            5,
            format!("session not found: {namespace}/{session_name}"),
        ));
    };

    let (run_id, generation) = match env_run_id {
        Some(run_id) => (run_id, env_generation),
        None => {
            let meta = session::read_meta(&dir).map_err(|e| fail(5, e.to_string()))?;
            (meta.run_id(), Some(meta.generation().as_u64()))
        }
    };

    let draft = EventDraft {
        id: None,
        kind,
        namespace,
        session: session_name,
        run_id,
        generation,
        source,
        block_id,
        parent_id,
        data,
        preview: None,
    };
    let mut writer = EventWriter::new(dir.path());
    writer
        .append(draft, opts.durable)
        .map_err(|e| fail(1, format!("event append failed: {e}")))?;
    Ok(())
}

/// Payload from exactly one of `--data` / `--data-file` / `--data-stdin`
/// (clap enforces mutual exclusion). Must be a JSON object (spec §1 `data`).
fn read_data(opts: &EmitOptions) -> Result<Option<serde_json::Value>, EmitFailure> {
    let raw: Option<String> = if let Some(inline) = &opts.data {
        Some(inline.clone())
    } else if let Some(path) = &opts.data_file {
        Some(std::fs::read_to_string(path).map_err(|e| {
            fail(
                2,
                format!("cannot read --data-file {}: {e}", path.display()),
            )
        })?)
    } else if opts.data_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| fail(2, format!("cannot read stdin: {e}")))?;
        Some(buf)
    } else {
        None
    };

    match raw {
        None => Ok(None),
        Some(text) => {
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| fail(2, format!("event data is not valid JSON: {e}")))?;
            if !value.is_object() {
                return Err(fail(2, "event data must be a JSON object"));
            }
            Ok(Some(value))
        }
    }
}

/// Session context: `--session <ns>/<name>` (bare name → default namespace),
/// else the `TENDR_SESSION`/`TENDR_NAMESPACE` environment of a supervised
/// run. The bool reports whether context came from the environment.
fn resolve_context(opts: &EmitOptions) -> Result<(Namespace, SessionName, bool), EmitFailure> {
    if let Some(spec) = &opts.session {
        let (ns, name) = match spec.split_once('/') {
            Some((ns, name)) => (ns, name),
            None => ("default", spec.as_str()),
        };
        let namespace =
            Namespace::new(ns).map_err(|e| fail(2, format!("invalid --session namespace: {e}")))?;
        let session =
            SessionName::new(name).map_err(|e| fail(2, format!("invalid --session name: {e}")))?;
        return Ok((namespace, session, false));
    }
    if let Ok(session) = std::env::var("TENDR_SESSION") {
        let ns = std::env::var("TENDR_NAMESPACE").unwrap_or_else(|_| "default".to_owned());
        let namespace =
            Namespace::new(&ns).map_err(|e| fail(3, format!("invalid TENDR_NAMESPACE: {e}")))?;
        let session = SessionName::new(&session)
            .map_err(|e| fail(3, format!("invalid TENDR_SESSION: {e}")))?;
        return Ok((namespace, session, true));
    }
    Err(fail(
        3,
        "no session context: pass --session <ns>/<name> or run inside a supervised session",
    ))
}
