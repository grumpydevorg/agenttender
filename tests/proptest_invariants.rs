use proptest::prelude::*;
use tender::log::LogLine;
use tender::model::ids::{EpochTimestamp, SessionName, SessionNameError};

// --- LogLine JSONL roundtrip ---

proptest! {
    #[test]
    fn logline_json_roundtrip(
        ts in 0f64..10_000_000_000f64,
        tag in prop_oneof![Just("O"), Just("E")],
        content in "[^\n\r]{0,200}",
    ) {
        let line = serde_json::to_string(&serde_json::json!({
            "ts": ts,
            "tag": tag,
            "content": content,
        })).unwrap();
        let parsed: LogLine = serde_json::from_str(&line).expect("should parse well-formed JSONL");

        prop_assert_eq!(parsed.tag.as_str(), tag);
        prop_assert_eq!(parsed.content.as_str(), Some(content.as_str()));

        let formatted = serde_json::to_string(&parsed).unwrap();
        let reparsed: LogLine = serde_json::from_str(&formatted).unwrap();
        prop_assert_eq!(reparsed, parsed);
    }

    #[test]
    fn logline_annotation_json_roundtrip(
        ts in 0f64..10_000_000_000f64,
        source in "[a-z]{1,12}\\.[a-z]{1,12}",
        event in "[a-z-]{1,20}",
        msg in "[^\n\r]{0,120}",
    ) {
        let line = serde_json::to_string(&serde_json::json!({
            "ts": ts,
            "tag": "A",
            "content": {
                "source": source,
                "event": event,
                "data": {
                    "msg": msg,
                }
            },
        })).unwrap();
        let parsed: LogLine = serde_json::from_str(&line).expect("should parse annotation JSONL");

        prop_assert_eq!(parsed.tag.as_str(), "A");
        prop_assert_eq!(parsed.content["source"].as_str(), Some(source.as_str()));
        prop_assert_eq!(parsed.content["event"].as_str(), Some(event.as_str()));
        prop_assert_eq!(parsed.content["data"]["msg"].as_str(), Some(msg.as_str()));

        let formatted = serde_json::to_string(&parsed).unwrap();
        let reparsed: LogLine = serde_json::from_str(&formatted).unwrap();
        prop_assert_eq!(reparsed, parsed);
    }

    #[test]
    fn logline_format_raw_is_content(
        ts in 0f64..10_000_000_000f64,
        tag in prop_oneof![Just("O"), Just("E")],
        content in "[^\n\r]{0,200}",
    ) {
        let line = serde_json::to_string(&serde_json::json!({
            "ts": ts,
            "tag": tag,
            "content": content,
        })).unwrap();
        let parsed: LogLine = serde_json::from_str(&line).unwrap();
        prop_assert_eq!(parsed.format_raw(), content);
    }

    #[test]
    fn logline_rejects_non_json(
        content in "[^\n\r]{1,50}",
    ) {
        prop_assert!(serde_json::from_str::<LogLine>(&content).is_err());
    }
}

// --- EpochTimestamp serde roundtrip ---

proptest! {
    #[test]
    fn epoch_timestamp_serde_roundtrip(secs in 1u64..10_000_000_000) {
        // Serialize as JSON, deserialize back
        let ts = serde_json::json!(secs.to_string());
        let parsed: EpochTimestamp = serde_json::from_value(ts).unwrap();
        let serialized = serde_json::to_value(parsed).unwrap();
        // EpochTimestamp serializes as string
        prop_assert_eq!(serialized.as_str().unwrap(), &secs.to_string());
    }

    #[test]
    fn epoch_timestamp_accepts_integer_and_string(secs in 1u64..10_000_000_000) {
        // String form
        let from_str: EpochTimestamp =
            serde_json::from_value(serde_json::json!(secs.to_string())).unwrap();
        // Integer form
        let from_int: EpochTimestamp =
            serde_json::from_value(serde_json::json!(secs)).unwrap();
        // Both should produce the same value
        prop_assert_eq!(from_str, from_int);
    }
}

// --- Cursor token roundtrip (spec §5.2) ---

proptest! {
    #[test]
    fn cursor_token_roundtrip(
        streams in proptest::collection::btree_map(
            "[a-z0-9-]{1,12}/[a-z0-9-]{1,12}/events/[a-f0-9-]{1,36}\\.jsonl",
            proptest::num::u64::ANY,
            0..8,
        ),
    ) {
        let token = tender::events::encode_cursor(&streams);
        // Opaque but URL-safe: survives query strings and shell args.
        prop_assert!(token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        let decoded = tender::events::decode_cursor(&token).unwrap();
        prop_assert_eq!(decoded, streams);
    }
}

// --- SessionName validation ---

proptest! {
    #[test]
    fn session_name_valid_names_roundtrip(
        name in "[a-zA-Z][a-zA-Z0-9_-]{0,50}"
    ) {
        let sn = SessionName::new(&name).unwrap();
        prop_assert_eq!(sn.as_str(), &name);

        // Serde roundtrip
        let json = serde_json::to_string(&sn).unwrap();
        let parsed: SessionName = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.as_str(), &name);
    }

    #[test]
    fn session_name_rejects_slashes(
        prefix in "[a-zA-Z]{1,10}",
        suffix in "[a-zA-Z]{1,10}",
    ) {
        let name = format!("{prefix}/{suffix}");
        prop_assert!(matches!(
            SessionName::new(&name),
            Err(SessionNameError::ContainsSlash)
        ));
    }

    #[test]
    fn session_name_rejects_dots(
        prefix in "[a-zA-Z]{1,10}",
        suffix in "[a-zA-Z]{1,10}",
    ) {
        let name = format!("{prefix}.{suffix}");
        prop_assert!(matches!(
            SessionName::new(&name),
            Err(SessionNameError::ContainsDot)
        ));
    }

    #[test]
    fn session_name_rejects_too_long(extra in 1usize..100) {
        let name = "a".repeat(255 + extra);
        prop_assert!(matches!(
            SessionName::new(&name),
            Err(SessionNameError::TooLong)
        ));
    }
}
