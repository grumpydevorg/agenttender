#[test]
fn launch_spec_io_mode_defaults_to_pipe() {
    let spec = tender::model::spec::LaunchSpec::new(vec!["echo".into()]).unwrap();
    assert_eq!(spec.io_mode, tender::model::spec::IoMode::Pipe);
}

#[test]
fn launch_spec_pty_mode_serializes() {
    let mut spec = tender::model::spec::LaunchSpec::new(vec!["bash".into()]).unwrap();
    spec.io_mode = tender::model::spec::IoMode::Pty;
    let json = serde_json::to_string(&spec).unwrap();
    assert!(json.contains("\"io_mode\":\"Pty\""), "json: {json}");
}

#[test]
fn launch_spec_without_io_mode_deserializes_as_pipe() {
    let json = r#"{"argv":["echo"],"stdin_mode":"None","exec_target":"None"}"#;
    let spec: tender::model::spec::LaunchSpec = serde_json::from_str(json).unwrap();
    assert_eq!(spec.io_mode, tender::model::spec::IoMode::Pipe);
}

#[test]
fn launch_spec_python_repl_deserializes() {
    let json = r#"{"argv":["python3"],"stdin_mode":"Pipe","exec_target":"PythonRepl"}"#;
    let spec: tender::model::spec::LaunchSpec = serde_json::from_str(json).unwrap();
    assert_eq!(
        spec.exec_target,
        tender::model::spec::ExecTarget::PythonRepl
    );
}

#[test]
fn launch_spec_duckdb_deserializes() {
    let json = r#"{"argv":["duckdb"],"stdin_mode":"Pipe","exec_target":"DuckDb"}"#;
    let spec: tender::model::spec::LaunchSpec = serde_json::from_str(json).unwrap();
    assert_eq!(spec.exec_target, tender::model::spec::ExecTarget::DuckDb);
}

#[test]
fn launch_spec_duckdb_serializes() {
    let mut spec = tender::model::spec::LaunchSpec::new(vec!["duckdb".into()]).unwrap();
    spec.exec_target = tender::model::spec::ExecTarget::DuckDb;
    spec.stdin_mode = tender::model::spec::StdinMode::Pipe;
    let json = serde_json::to_string(&spec).unwrap();
    assert!(json.contains("\"DuckDb\""), "json: {json}");
}

// --- boundary metadata ---

#[test]
fn launch_spec_boundary_defaults_to_none() {
    let spec = tender::model::spec::LaunchSpec::new(vec!["echo".into()]).unwrap();
    assert!(spec.boundary.is_none());
}

#[test]
fn launch_spec_without_boundary_omits_the_field() {
    // A None boundary must not appear in the serialized form — this keeps
    // canonical_hash (and thus idempotent-start matching) stable for the
    // overwhelming majority of specs that declare no boundary.
    let spec = tender::model::spec::LaunchSpec::new(vec!["echo".into()]).unwrap();
    let json = serde_json::to_string(&spec).unwrap();
    assert!(
        !json.contains("boundary"),
        "json unexpectedly has boundary: {json}"
    );
}

#[test]
fn old_launch_spec_json_without_boundary_deserializes_cleanly() {
    // meta.json / launch_spec.json written before this feature existed.
    let json = r#"{"argv":["echo"],"stdin_mode":"None","exec_target":"None"}"#;
    let spec: tender::model::spec::LaunchSpec = serde_json::from_str(json).unwrap();
    assert!(spec.boundary.is_none());
}

#[test]
fn launch_spec_boundary_round_trips() {
    use tender::model::boundary::{Boundary, BoundaryContext, BoundaryKind};

    let mut spec = tender::model::spec::LaunchSpec::new(vec!["bash".into()]).unwrap();
    spec.boundary = Some(BoundaryContext {
        current: Boundary {
            kind: BoundaryKind::Container,
            label: "my-image:latest".to_owned(),
        },
        parents: vec![Boundary {
            kind: BoundaryKind::Host,
            label: "data-box".to_owned(),
        }],
    });
    let json = serde_json::to_string(&spec).unwrap();
    let back: tender::model::spec::LaunchSpec = serde_json::from_str(&json).unwrap();
    assert_eq!(back.boundary, spec.boundary);
}

#[test]
fn none_boundary_does_not_change_canonical_hash() {
    // Explicitly setting boundary to None must hash identically to never
    // touching the field — backward-compatible idempotent matching.
    let a = tender::model::spec::LaunchSpec::new(vec!["echo".into(), "hi".into()]).unwrap();
    let mut b = tender::model::spec::LaunchSpec::new(vec!["echo".into(), "hi".into()]).unwrap();
    b.boundary = None;
    assert_eq!(a.canonical_hash(), b.canonical_hash());
}
