use std::num::{NonZeroI32, NonZeroU32};
use tender::model::ids::{EpochTimestamp, Generation, ProcessIdentity, RunId, SessionName};
use tender::model::meta::Meta;
use tender::model::spec::LaunchSpec;
use tender::model::state::{ExitReason, RunStatus, SidecarStep};

fn test_sidecar() -> ProcessIdentity {
    ProcessIdentity {
        pid: NonZeroU32::new(100).unwrap(),
        start_time_ns: 1000,
    }
}

fn test_child() -> ProcessIdentity {
    ProcessIdentity {
        pid: NonZeroU32::new(101).unwrap(),
        start_time_ns: 2000,
    }
}

fn test_spec() -> LaunchSpec {
    LaunchSpec::new(vec!["echo".into(), "hello".into()]).unwrap()
}

fn starting_meta() -> Meta {
    Meta::new_starting(
        SessionName::new("test-job").unwrap(),
        RunId::new(),
        Generation::first(),
        test_spec(),
        test_sidecar(),
        EpochTimestamp::now(),
    )
}

// === Legal transitions ===

#[test]
fn starting_to_running() {
    let mut meta = starting_meta();
    assert!(meta.transition_running(test_child()).is_ok());
    assert!(matches!(meta.status(), RunStatus::Running { .. }));
    assert!(meta.status().child().is_some());
    assert!(meta.status().ended_at().is_none());
}

#[test]
fn starting_to_spawn_failed() {
    let mut meta = starting_meta();
    assert!(meta.transition_spawn_failed(EpochTimestamp::now()).is_ok());
    assert!(matches!(meta.status(), RunStatus::SpawnFailed { .. }));
    assert!(meta.status().child().is_none());
    assert!(meta.status().ended_at().is_some());
}

#[test]
fn running_to_exited_ok() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    assert!(
        meta.transition_exited(ExitReason::ExitedOk, EpochTimestamp::now())
            .is_ok()
    );
    assert!(matches!(
        meta.status(),
        RunStatus::Exited {
            how: ExitReason::ExitedOk,
            ..
        }
    ));
    assert!(meta.status().child().is_some());
    assert!(meta.status().ended_at().is_some());
}

#[test]
fn running_to_exited_error() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    let code = NonZeroI32::new(42).unwrap();
    assert!(
        meta.transition_exited(ExitReason::ExitedError { code }, EpochTimestamp::now())
            .is_ok()
    );
    match meta.status() {
        RunStatus::Exited {
            how: ExitReason::ExitedError { code },
            ..
        } => {
            assert_eq!(code.get(), 42);
        }
        other => panic!("expected ExitedError, got {other:?}"),
    }
}

#[test]
fn running_to_killed() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    assert!(
        meta.transition_exited(ExitReason::Killed, EpochTimestamp::now())
            .is_ok()
    );
    assert!(matches!(
        meta.status(),
        RunStatus::Exited {
            how: ExitReason::Killed,
            ..
        }
    ));
}

#[test]
fn running_to_killed_forced() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    assert!(
        meta.transition_exited(ExitReason::KilledForced, EpochTimestamp::now())
            .is_ok()
    );
    assert!(matches!(
        meta.status(),
        RunStatus::Exited {
            how: ExitReason::KilledForced,
            ..
        }
    ));
}

#[test]
fn running_to_timed_out() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    assert!(
        meta.transition_exited(ExitReason::TimedOut, EpochTimestamp::now())
            .is_ok()
    );
    assert!(matches!(
        meta.status(),
        RunStatus::Exited {
            how: ExitReason::TimedOut,
            ..
        }
    ));
}

// === Reconciliation ===

#[test]
fn starting_to_sidecar_lost() {
    let mut meta = starting_meta();
    assert!(meta.reconcile_sidecar_lost(EpochTimestamp::now()).is_ok());
    assert!(matches!(
        meta.status(),
        RunStatus::SidecarLost { child: None, .. }
    ));
}

#[test]
fn running_to_sidecar_lost() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    assert!(meta.reconcile_sidecar_lost(EpochTimestamp::now()).is_ok());
    assert!(matches!(
        meta.status(),
        RunStatus::SidecarLost { child: Some(_), .. }
    ));
}

// === Illegal transitions ===

#[test]
fn cannot_transition_terminal_to_running() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    meta.transition_exited(ExitReason::ExitedOk, EpochTimestamp::now())
        .unwrap();
    assert!(meta.transition_running(test_child()).is_err());
}

#[test]
fn cannot_transition_terminal_to_terminal() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    meta.transition_exited(ExitReason::ExitedOk, EpochTimestamp::now())
        .unwrap();
    assert!(
        meta.transition_exited(ExitReason::Killed, EpochTimestamp::now())
            .is_err()
    );
}

#[test]
fn cannot_transition_running_to_running() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    assert!(meta.transition_running(test_child()).is_err());
}

#[test]
fn cannot_exited_from_starting() {
    let mut meta = starting_meta();
    assert!(
        meta.transition_exited(ExitReason::ExitedOk, EpochTimestamp::now())
            .is_err()
    );
}

#[test]
fn cannot_spawn_fail_from_running() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    assert!(meta.transition_spawn_failed(EpochTimestamp::now()).is_err());
}

#[test]
fn cannot_reconcile_terminal() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    meta.transition_exited(ExitReason::ExitedOk, EpochTimestamp::now())
        .unwrap();
    assert!(meta.reconcile_sidecar_lost(EpochTimestamp::now()).is_err());
}

// === Type-level invariants ===

#[test]
fn starting_has_no_child() {
    let meta = starting_meta();
    assert!(meta.status().child().is_none());
    assert!(matches!(meta.status(), RunStatus::Starting));
}

#[test]
fn running_has_child() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    assert!(meta.status().child().is_some());
}

#[test]
fn terminal_has_ended_at() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    meta.transition_exited(ExitReason::ExitedOk, EpochTimestamp::now())
        .unwrap();
    assert!(meta.status().ended_at().is_some());
}

#[test]
fn non_terminal_has_no_ended_at() {
    let meta = starting_meta();
    assert!(meta.status().ended_at().is_none());

    let mut meta2 = starting_meta();
    meta2.transition_running(test_child()).unwrap();
    assert!(meta2.status().ended_at().is_none());
}

#[test]
fn spawn_failed_has_no_child() {
    let mut meta = starting_meta();
    meta.transition_spawn_failed(EpochTimestamp::now()).unwrap();
    assert!(meta.status().child().is_none());
}

#[test]
fn exited_preserves_child() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    meta.transition_exited(ExitReason::Killed, EpochTimestamp::now())
        .unwrap();
    assert_eq!(meta.status().child(), Some(&test_child()));
}

// === Serde round-trip ===

#[test]
fn meta_serde_roundtrip_starting() {
    let meta = starting_meta();
    let json = serde_json::to_string_pretty(&meta).unwrap();
    let back: Meta = serde_json::from_str(&json).unwrap();
    assert!(matches!(back.status(), RunStatus::Starting));
    assert_eq!(back.schema_version(), Meta::SCHEMA_VERSION);
}

#[test]
fn meta_serde_roundtrip_running() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    let json = serde_json::to_string_pretty(&meta).unwrap();
    let back: Meta = serde_json::from_str(&json).unwrap();
    assert!(matches!(back.status(), RunStatus::Running { .. }));
    assert!(back.status().child().is_some());
}

#[test]
fn meta_serde_roundtrip_exited_error() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    let code = NonZeroI32::new(1).unwrap();
    meta.transition_exited(ExitReason::ExitedError { code }, EpochTimestamp::now())
        .unwrap();
    let json = serde_json::to_string_pretty(&meta).unwrap();
    let back: Meta = serde_json::from_str(&json).unwrap();
    match back.status() {
        RunStatus::Exited {
            how: ExitReason::ExitedError { code },
            ..
        } => {
            assert_eq!(code.get(), 1);
        }
        other => panic!("expected ExitedError, got {other:?}"),
    }
    assert!(back.status().ended_at().is_some());
}

#[test]
fn meta_serde_roundtrip_spawn_failed() {
    let mut meta = starting_meta();
    meta.transition_spawn_failed(EpochTimestamp::now()).unwrap();
    let json = serde_json::to_string_pretty(&meta).unwrap();
    let back: Meta = serde_json::from_str(&json).unwrap();
    assert!(matches!(back.status(), RunStatus::SpawnFailed { .. }));
    assert!(back.status().child().is_none());
}

#[test]
fn meta_serde_roundtrip_sidecar_lost() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    meta.reconcile_sidecar_lost(EpochTimestamp::now()).unwrap();
    let json = serde_json::to_string_pretty(&meta).unwrap();
    let back: Meta = serde_json::from_str(&json).unwrap();
    assert!(matches!(
        back.status(),
        RunStatus::SidecarLost { child: Some(_), .. }
    ));
}

#[test]
fn meta_rejects_wrong_schema_version() {
    let meta = starting_meta();
    let mut json: serde_json::Value = serde_json::to_value(&meta).unwrap();
    json["schema_version"] = serde_json::Value::from(99);
    let result: Result<Meta, _> = serde_json::from_value(json);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("schema version"));
}

// === DependencyFailed transitions ===

#[test]
fn starting_to_dependency_failed() {
    let mut meta = starting_meta();
    assert!(!meta.status().is_terminal());
    meta.transition_dependency_failed(
        EpochTimestamp::now(),
        tender::model::dep_fail::DepFailReason::Failed,
    )
    .unwrap();
    assert!(meta.status().is_terminal());
    assert!(matches!(meta.status(), RunStatus::DependencyFailed { .. }));
}

#[test]
fn running_to_dependency_failed_is_illegal() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    let result = meta.transition_dependency_failed(
        EpochTimestamp::now(),
        tender::model::dep_fail::DepFailReason::Failed,
    );
    assert!(result.is_err());
}

// === Launch spec hash ===

#[test]
fn canonical_hash_stable() {
    let spec = test_spec();
    let h1 = spec.canonical_hash();
    let h2 = spec.canonical_hash();
    assert_eq!(h1, h2);
    assert!(h1.starts_with("sha256:"));
}

#[test]
fn canonical_hash_different_for_different_specs() {
    let spec1 = test_spec();
    let spec2 = LaunchSpec::new(vec!["different".into()]).unwrap();
    assert_ne!(spec1.canonical_hash(), spec2.canonical_hash());
}

#[test]
fn canonical_hash_includes_env() {
    let spec1 = test_spec();
    let mut spec2 = test_spec();
    spec2.env.insert("KEY".into(), "VALUE".into());
    assert_ne!(spec1.canonical_hash(), spec2.canonical_hash());
}

#[test]
fn canonical_hash_includes_timeout() {
    let spec1 = test_spec();
    let mut spec2 = test_spec();
    spec2.timeout_s = Some(3600);
    assert_ne!(spec1.canonical_hash(), spec2.canonical_hash());
}

// === Launch spec validation ===

#[test]
fn launch_spec_rejects_empty_argv() {
    assert!(LaunchSpec::new(vec![]).is_err());
}

#[test]
fn launch_spec_rejects_empty_argv_on_deserialize() {
    let json = r#"{"argv":[],"stdin_mode":"None"}"#;
    let result: Result<LaunchSpec, _> = serde_json::from_str(json);
    assert!(result.is_err());
}

// === Warnings ===

#[test]
fn meta_warnings_empty_not_serialized() {
    let meta = starting_meta();
    let json = serde_json::to_string(&meta).unwrap();
    assert!(
        !json.contains("warnings"),
        "empty warnings should be omitted from JSON"
    );
}

#[test]
fn meta_warnings_roundtrip() {
    let mut meta = starting_meta();
    meta.add_warning("log capture: stdout capture thread panicked".into());
    meta.add_warning("stdin forwarding: child stdin closed".into());
    let json = serde_json::to_string_pretty(&meta).unwrap();
    assert!(json.contains("warnings"));
    let back: Meta = serde_json::from_str(&json).unwrap();
    assert_eq!(back.warnings().len(), 2);
    assert_eq!(
        back.warnings()[0],
        "log capture: stdout capture thread panicked"
    );
    assert_eq!(back.warnings()[1], "stdin forwarding: child stdin closed");
}

#[test]
fn meta_deserialize_without_warnings_field() {
    // JSON without "warnings" key should deserialize fine (defaults to empty vec)
    let meta = starting_meta();
    let mut val: serde_json::Value = serde_json::to_value(&meta).unwrap();
    // Ensure no warnings key exists
    val.as_object_mut().unwrap().remove("warnings");
    let back: Meta = serde_json::from_value(val).unwrap();
    assert!(back.warnings().is_empty());
}

// === Hash is never stale ===

#[test]
fn meta_hash_computed_on_demand() {
    let meta = starting_meta();
    let h1 = meta.launch_spec_hash();
    let h2 = meta.launch_spec_hash();
    assert_eq!(h1, h2);
    assert_eq!(h1, meta.launch_spec().canonical_hash());
}

// === SidecarFailed: the sidecar's own terminal record of a supervision failure ===

#[test]
fn running_to_sidecar_failed_serializes_reason_and_step() {
    let mut meta = starting_meta();
    meta.transition_running(test_child()).unwrap();
    meta.transition_sidecar_failed(test_child(), SidecarStep::OutputLog, EpochTimestamp::now())
        .unwrap();
    let json: serde_json::Value = serde_json::to_value(&meta).unwrap();
    assert_eq!(json["status"], "Exited");
    assert_eq!(json["reason"], "SidecarFailed");
    assert_eq!(json["step"], "output_log");
    assert_eq!(json["transition_provenance"]["kind"], "direct");

    let back: Meta = serde_json::from_value(json).unwrap();
    assert_eq!(back.status(), meta.status());
}

#[test]
fn every_sidecar_step_round_trips_by_wire_name() {
    for step in [
        SidecarStep::Breadcrumb,
        SidecarStep::StdinTransport,
        SidecarStep::AttachBind,
        SidecarStep::RunningMeta,
        SidecarStep::Readiness,
        SidecarStep::OutputLog,
        SidecarStep::ChildWait,
    ] {
        let wire = serde_json::to_value(step).unwrap();
        assert_eq!(wire, step.as_str(), "serde and as_str agree");
        assert_eq!(SidecarStep::from_wire(step.as_str()), Some(step));
    }
    assert_eq!(SidecarStep::from_wire("not_a_step"), None);
}

#[test]
fn starting_to_sidecar_failed_carries_the_spawned_child() {
    let mut meta = starting_meta();
    meta.transition_sidecar_failed(
        test_child(),
        SidecarStep::StdinTransport,
        EpochTimestamp::now(),
    )
    .unwrap();
    assert_eq!(meta.status().child(), Some(&test_child()));
    assert!(meta.status().is_terminal());
}

#[test]
fn cannot_sidecar_fail_a_terminal_run() {
    let mut meta = starting_meta();
    meta.reconcile_sidecar_lost(EpochTimestamp::now()).unwrap();
    assert!(
        meta.transition_sidecar_failed(test_child(), SidecarStep::ChildWait, EpochTimestamp::now())
            .is_err()
    );
}
