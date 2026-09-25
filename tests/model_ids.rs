use std::num::NonZeroU32;
use tendr::model::ids::{
    EpochTimestamp, Generation, Namespace, NamespaceError, ProcessIdentity, RunId, SessionName,
    SessionNameError, Source, SourceError,
};

#[test]
fn run_id_uniqueness() {
    let a = RunId::new();
    let b = RunId::new();
    assert_ne!(a, b);
}

#[test]
fn run_id_display_is_uuid_format() {
    let id = RunId::new();
    let s = id.to_string();
    assert_eq!(s.len(), 36);
    assert_eq!(s.chars().filter(|c| *c == '-').count(), 4);
}

#[test]
fn run_id_serde_roundtrip() {
    let id = RunId::new();
    let json = serde_json::to_string(&id).unwrap();
    let back: RunId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, back);
}

#[test]
fn run_id_rejects_non_v7_uuid() {
    // Nil UUID (v0) should be rejected
    let nil = uuid::Uuid::nil();
    let json = serde_json::to_string(&nil).unwrap();
    let result: Result<RunId, _> = serde_json::from_str(&json);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("v7"));
}

#[test]
fn generation_starts_at_one() {
    let g = Generation::first();
    assert_eq!(g.as_u64(), 1);
}

#[test]
fn generation_increments() {
    let g = Generation::first().next().next();
    assert_eq!(g.as_u64(), 3);
}

#[test]
fn generation_serde_roundtrip() {
    let g = Generation::first().next();
    let json = serde_json::to_string(&g).unwrap();
    let back: Generation = serde_json::from_str(&json).unwrap();
    assert_eq!(g, back);
}

#[test]
fn generation_rejects_zero() {
    let result: Result<Generation, _> = serde_json::from_str("0");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("zero"));
}

#[test]
fn session_name_valid() {
    assert!(SessionName::new("upload").is_ok());
    assert!(SessionName::new("my-job").is_ok());
    assert!(SessionName::new("job_123").is_ok());
    assert!(SessionName::new("a").is_ok());
}

#[test]
fn session_name_empty_rejected() {
    assert!(matches!(
        SessionName::new("").unwrap_err(),
        SessionNameError::Empty
    ));
}

#[test]
fn session_name_slash_rejected() {
    assert!(matches!(
        SessionName::new("a/b").unwrap_err(),
        SessionNameError::ContainsSlash
    ));
}

#[test]
fn session_name_dot_rejected() {
    assert!(matches!(
        SessionName::new("a.b").unwrap_err(),
        SessionNameError::ContainsDot
    ));
}

#[test]
fn session_name_whitespace_rejected() {
    assert!(matches!(
        SessionName::new("a b").unwrap_err(),
        SessionNameError::ContainsWhitespace
    ));
    assert!(matches!(
        SessionName::new("a\tb").unwrap_err(),
        SessionNameError::ContainsWhitespace
    ));
}

#[test]
fn session_name_too_long_rejected() {
    let long_name = "a".repeat(256);
    assert!(matches!(
        SessionName::new(&long_name).unwrap_err(),
        SessionNameError::TooLong
    ));
    // 255 bytes is the limit — should succeed
    let max_name = "a".repeat(255);
    assert!(SessionName::new(&max_name).is_ok());
}

#[test]
fn session_name_underscore_prefix_rejected() {
    assert!(matches!(
        SessionName::new("_sidecar").unwrap_err(),
        SessionNameError::StartsWithUnderscore
    ));
}

#[test]
fn session_name_serde_roundtrip() {
    let name = SessionName::new("upload").unwrap();
    let json = serde_json::to_string(&name).unwrap();
    let back: SessionName = serde_json::from_str(&json).unwrap();
    assert_eq!(name, back);
}

#[test]
fn session_name_rejects_invalid_on_deserialize() {
    // Empty
    let result: Result<SessionName, _> = serde_json::from_str(r#""""#);
    assert!(result.is_err());

    // Contains slash
    let result: Result<SessionName, _> = serde_json::from_str(r#""a/b""#);
    assert!(result.is_err());

    // Starts with underscore
    let result: Result<SessionName, _> = serde_json::from_str(r#""_bad""#);
    assert!(result.is_err());
}

#[test]
fn process_identity_serde_roundtrip() {
    let id = ProcessIdentity {
        pid: NonZeroU32::new(1234).unwrap(),
        start_time_ns: 1_710_612_345_000_000_000,
    };
    let json = serde_json::to_string(&id).unwrap();
    let back: ProcessIdentity = serde_json::from_str(&json).unwrap();
    assert_eq!(id, back);
}

// === EpochTimestamp ===

#[test]
fn epoch_timestamp_now_is_reasonable() {
    let ts = EpochTimestamp::now();
    // Must be after year ~2023 (1_700_000_000 epoch seconds)
    assert!(ts.as_secs() > 1_700_000_000);
}

#[test]
fn epoch_timestamp_serde_roundtrip() {
    let ts = EpochTimestamp::from_secs(1_773_653_954);
    let json = serde_json::to_string(&ts).unwrap();
    let back: EpochTimestamp = serde_json::from_str(&json).unwrap();
    assert_eq!(ts, back);
}

#[test]
fn epoch_timestamp_deserialize_from_string() {
    let ts: EpochTimestamp = serde_json::from_str(r#""1773653954""#).unwrap();
    assert_eq!(ts.as_secs(), 1_773_653_954);
}

#[test]
fn epoch_timestamp_deserialize_from_integer() {
    let ts: EpochTimestamp = serde_json::from_str("1773653954").unwrap();
    assert_eq!(ts.as_secs(), 1_773_653_954);
}

#[test]
fn epoch_timestamp_deserialize_rejects_garbage() {
    let result: Result<EpochTimestamp, _> = serde_json::from_str(r#""not-a-number""#);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("invalid epoch timestamp")
    );
}

#[test]
fn epoch_timestamp_display() {
    let ts = EpochTimestamp::from_secs(1_773_653_954);
    assert_eq!(format!("{ts}"), "1773653954");
}

#[test]
fn epoch_timestamp_serializes_as_string() {
    let ts = EpochTimestamp::from_secs(1_773_653_954);
    let json = serde_json::to_string(&ts).unwrap();
    // Must be a quoted string, not bare integer
    assert_eq!(json, r#""1773653954""#);
}

// === Namespace ===

#[test]
fn namespace_valid() {
    assert!(Namespace::new("default").is_ok());
    assert!(Namespace::new("my-workspace").is_ok());
    assert!(Namespace::new("prod_123").is_ok());
    assert!(Namespace::new("a").is_ok());
}

#[test]
fn namespace_default() {
    let ns = Namespace::default_namespace();
    assert_eq!(ns.as_str(), "default");
}

#[test]
fn namespace_empty_rejected() {
    assert!(matches!(
        Namespace::new("").unwrap_err(),
        NamespaceError::Empty
    ));
}

#[test]
fn namespace_slash_rejected() {
    assert!(matches!(
        Namespace::new("a/b").unwrap_err(),
        NamespaceError::ContainsSlash
    ));
}

#[test]
fn namespace_dot_rejected() {
    assert!(matches!(
        Namespace::new("a.b").unwrap_err(),
        NamespaceError::ContainsDot
    ));
}

#[test]
fn namespace_whitespace_rejected() {
    assert!(matches!(
        Namespace::new("a b").unwrap_err(),
        NamespaceError::ContainsWhitespace
    ));
    assert!(matches!(
        Namespace::new("a\tb").unwrap_err(),
        NamespaceError::ContainsWhitespace
    ));
}

#[test]
fn namespace_too_long_rejected() {
    let long_name = "a".repeat(256);
    assert!(matches!(
        Namespace::new(&long_name).unwrap_err(),
        NamespaceError::TooLong
    ));
    // 255 bytes is the limit — should succeed
    let max_name = "a".repeat(255);
    assert!(Namespace::new(&max_name).is_ok());
}

#[test]
fn namespace_underscore_prefix_rejected() {
    assert!(matches!(
        Namespace::new("_hidden").unwrap_err(),
        NamespaceError::StartsWithUnderscore
    ));
}

#[test]
fn namespace_serde_roundtrip() {
    let ns = Namespace::new("production").unwrap();
    let json = serde_json::to_string(&ns).unwrap();
    let back: Namespace = serde_json::from_str(&json).unwrap();
    assert_eq!(ns, back);
}

#[test]
fn namespace_rejects_invalid_on_deserialize() {
    // Empty
    let result: Result<Namespace, _> = serde_json::from_str(r#""""#);
    assert!(result.is_err());

    // Contains slash
    let result: Result<Namespace, _> = serde_json::from_str(r#""a/b""#);
    assert!(result.is_err());

    // Starts with underscore
    let result: Result<Namespace, _> = serde_json::from_str(r#""_bad""#);
    assert!(result.is_err());
}

#[test]
fn namespace_display() {
    let ns = Namespace::new("staging").unwrap();
    assert_eq!(format!("{ns}"), "staging");
}

// --- Source ---

#[test]
fn source_valid() {
    assert!(Source::new("cmux.claude-hook").is_ok());
    assert!(Source::new("my-tool.pre-commit").is_ok());
    assert!(Source::new("ci.build-observer").is_ok());
    assert!(Source::new("a.b").is_ok());
}

#[test]
fn source_empty_rejected() {
    assert!(matches!(Source::new("").unwrap_err(), SourceError::Empty));
}

#[test]
fn source_no_dot_rejected() {
    assert!(matches!(
        Source::new("nodot").unwrap_err(),
        SourceError::NoDot
    ));
}

#[test]
fn source_reserved_tendr_prefix_rejected() {
    assert!(matches!(
        Source::new("tendr.sidecar").unwrap_err(),
        SourceError::ReservedPrefix
    ));
    assert!(matches!(
        Source::new("tendr.anything").unwrap_err(),
        SourceError::ReservedPrefix
    ));
}

#[test]
fn source_invalid_char_rejected() {
    assert!(matches!(
        Source::new("a.b/c").unwrap_err(),
        SourceError::InvalidChar('/')
    ));
    assert!(matches!(
        Source::new("a.b c").unwrap_err(),
        SourceError::InvalidChar(' ')
    ));
}

#[test]
fn source_empty_segment_rejected() {
    assert!(matches!(
        Source::new(".foo").unwrap_err(),
        SourceError::EmptySegment
    ));
    assert!(matches!(
        Source::new("foo.").unwrap_err(),
        SourceError::EmptySegment
    ));
    assert!(matches!(
        Source::new("foo..bar").unwrap_err(),
        SourceError::EmptySegment
    ));
}

#[test]
fn source_too_long_rejected() {
    let long = format!("a.{}", "b".repeat(127));
    assert!(matches!(
        Source::new(&long).unwrap_err(),
        SourceError::TooLong
    ));
}

#[test]
fn source_serde_roundtrip() {
    let src = Source::new("test.source").unwrap();
    let json = serde_json::to_string(&src).unwrap();
    assert_eq!(json, r#""test.source""#);
    let parsed: Source = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.as_str(), "test.source");
}

#[test]
fn source_display() {
    let src = Source::new("cmux.hook").unwrap();
    assert_eq!(format!("{src}"), "cmux.hook");
}
