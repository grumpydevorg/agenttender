//! Parse `#tendr:` directives from script headers.
//!
//! Scans comment lines at the top of a script file (after an optional shebang)
//! for `#tendr: key=value` directives that configure session launch parameters.

use std::collections::BTreeMap;
use std::path::Path;

/// Parsed directives from a script header.
#[derive(Debug, Default, PartialEq)]
pub struct Directives {
    pub namespace: Option<String>,
    pub timeout: Option<u64>,
    pub on_exit: Vec<String>,
    pub stdin_pipe: bool,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub replace: bool,
    pub session: Option<String>,
    pub detach: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DirectiveError {
    #[error("unknown directive: {0}")]
    Unknown(String),
    #[error("duplicate directive: {0} (only one allowed)")]
    Duplicate(String),
    #[error("invalid timeout value: {0}")]
    InvalidTimeout(String),
    #[error("invalid stdin value: expected 'pipe', got: {0}")]
    InvalidStdin(String),
    #[error("invalid namespace: {0}")]
    InvalidNamespace(String),
    #[error("invalid session name: {0}")]
    InvalidSession(String),
}

/// Parse directives from script content (as a string).
///
/// Scanning rules:
/// - Skip the first line if it starts with `#!` (shebang)
/// - Continue scanning lines that are blank or start with `#`
/// - Stop at the first line that is neither blank nor a `#` comment
/// - Extract `#tendr: key=value` or `#tendr: key` (for boolean flags)
pub fn parse_directives(content: &str) -> Result<Directives, DirectiveError> {
    let mut directives = Directives::default();
    let mut lines = content.lines();

    // Skip shebang if present.
    if let Some(first) = lines.next() {
        if !first.starts_with("#!") {
            // Not a shebang — process this line as a potential directive.
            process_line(first, &mut directives)?;
        }
    }

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with('#') {
            break; // First non-blank, non-comment line — stop scanning.
        }
        process_line(trimmed, &mut directives)?;
    }

    Ok(directives)
}

fn process_line(line: &str, directives: &mut Directives) -> Result<(), DirectiveError> {
    let trimmed = line.trim();

    // Must match "#tendr: " (with space after colon).
    let Some(rest) = trimmed.strip_prefix("#tendr: ") else {
        return Ok(()); // Regular comment, skip.
    };

    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(());
    }

    // Split on first `=` for key=value, or treat as boolean flag.
    if let Some((key, value)) = rest.split_once('=') {
        let key = key.trim();
        let value = value.trim();
        apply_directive(key, Some(value), directives)
    } else {
        apply_directive(rest, None, directives)
    }
}

fn apply_directive(
    key: &str,
    value: Option<&str>,
    d: &mut Directives,
) -> Result<(), DirectiveError> {
    match key {
        "namespace" => {
            let v = value.unwrap_or("");
            if d.namespace.is_some() {
                return Err(DirectiveError::Duplicate("namespace".into()));
            }
            // Validate namespace at parse time — fail fast on bad values.
            crate::model::ids::Namespace::new(v)
                .map_err(|e| DirectiveError::InvalidNamespace(format!("{v}: {e}")))?;
            d.namespace = Some(v.to_string());
        }
        "timeout" => {
            let v = value.unwrap_or("");
            if d.timeout.is_some() {
                return Err(DirectiveError::Duplicate("timeout".into()));
            }
            d.timeout = Some(
                v.parse::<u64>()
                    .map_err(|_| DirectiveError::InvalidTimeout(v.to_string()))?,
            );
        }
        "on-exit" => {
            let v = value.unwrap_or("");
            d.on_exit.push(v.to_string());
        }
        "stdin" => {
            let v = value.unwrap_or("");
            if v != "pipe" {
                return Err(DirectiveError::InvalidStdin(v.to_string()));
            }
            if d.stdin_pipe {
                return Err(DirectiveError::Duplicate("stdin".into()));
            }
            d.stdin_pipe = true;
        }
        "cwd" => {
            let v = value.unwrap_or("");
            if d.cwd.is_some() {
                return Err(DirectiveError::Duplicate("cwd".into()));
            }
            d.cwd = Some(v.to_string());
        }
        "env" => {
            // env=KEY=VALUE — the value contains the full KEY=VALUE string.
            let v = value.unwrap_or("");
            // We store the raw KEY=VALUE string; cmd_start will parse it.
            d.env.insert(
                v.split_once('=')
                    .map(|(k, _)| k.to_string())
                    .unwrap_or_else(|| v.to_string()),
                v.to_string(),
            );
        }
        "replace" => {
            if d.replace {
                return Err(DirectiveError::Duplicate("replace".into()));
            }
            d.replace = true;
        }
        "session" => {
            let v = value.unwrap_or("");
            if d.session.is_some() {
                return Err(DirectiveError::Duplicate("session".into()));
            }
            // Validate session name at parse time — fail fast on bad values.
            crate::model::ids::SessionName::new(v)
                .map_err(|e| DirectiveError::InvalidSession(format!("{v}: {e}")))?;
            d.session = Some(v.to_string());
        }
        "detach" => {
            if d.detach {
                return Err(DirectiveError::Duplicate("detach".into()));
            }
            d.detach = true;
        }
        _ => return Err(DirectiveError::Unknown(key.to_string())),
    }
    Ok(())
}

/// Derive a session name from a script file path.
///
/// Steps:
/// 1. Take basename
/// 2. Strip the last extension
/// 3. Replace dots with hyphens
/// 4. Strip leading underscores
/// 5. Truncate to 255 chars
/// 6. Validate against SessionName rules
pub fn derive_session_name(script_path: &Path) -> Result<String, String> {
    let basename = script_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "cannot extract filename from script path".to_string())?;

    // Strip last extension.
    let stem = if let Some(pos) = basename.rfind('.') {
        if pos == 0 {
            // File starts with dot (e.g., ".hidden.sh") — strip the last extension.
            let without_leading_dot = &basename[1..];
            if let Some(inner_pos) = without_leading_dot.rfind('.') {
                &without_leading_dot[..inner_pos]
            } else {
                without_leading_dot
            }
        } else {
            &basename[..pos]
        }
    } else {
        basename // No extension (e.g., "Makefile")
    };

    // Replace dots with hyphens.
    let mut name = stem.replace('.', "-");

    // Strip leading underscores and hyphens (from dot-prefixed files like .hidden.sh).
    name = name.trim_start_matches(['_', '-']).to_string();

    // Truncate to 255 bytes at a char boundary (avoids panic on multi-byte UTF-8).
    if name.len() > 255 {
        let mut end = 255;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
    }

    if name.is_empty() {
        return Err(format!(
            "cannot derive session name from '{basename}'; use #tendr: session=NAME"
        ));
    }

    // Validate against SessionName rules (dots already replaced, underscores stripped).
    use crate::model::ids::SessionName;
    match SessionName::new(&name) {
        Ok(_) => Ok(name),
        Err(e) => Err(format!(
            "derived session name '{name}' is invalid: {e}; use #tendr: session=NAME"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_directives() {
        let content = "\
#!/usr/bin/env -S tendr run
#tendr: namespace=builds
#tendr: timeout=3600
#tendr: on-exit=notify-done

make -j8
";
        let d = parse_directives(content).unwrap();
        assert_eq!(d.namespace.as_deref(), Some("builds"));
        assert_eq!(d.timeout, Some(3600));
        assert_eq!(d.on_exit, vec!["notify-done"]);
    }

    #[test]
    fn parse_no_shebang() {
        let content = "\
#tendr: namespace=test
#tendr: timeout=60

echo hello
";
        let d = parse_directives(content).unwrap();
        assert_eq!(d.namespace.as_deref(), Some("test"));
        assert_eq!(d.timeout, Some(60));
    }

    #[test]
    fn parse_stops_at_non_comment() {
        let content = "\
#!/bin/bash
#tendr: namespace=a

echo hello
#tendr: namespace=b
";
        let d = parse_directives(content).unwrap();
        assert_eq!(d.namespace.as_deref(), Some("a"));
    }

    #[test]
    fn parse_unknown_directive_errors() {
        let content = "#tendr: timout=30\necho hi\n";
        let err = parse_directives(content).unwrap_err();
        assert!(matches!(err, DirectiveError::Unknown(k) if k == "timout"));
    }

    #[test]
    fn parse_duplicate_non_repeatable_errors() {
        let content = "#tendr: timeout=30\n#tendr: timeout=60\necho hi\n";
        let err = parse_directives(content).unwrap_err();
        assert!(matches!(err, DirectiveError::Duplicate(k) if k == "timeout"));
    }

    #[test]
    fn parse_repeatable_keys() {
        let content = "#tendr: on-exit=cmd1\n#tendr: on-exit=cmd2\necho hi\n";
        let d = parse_directives(content).unwrap();
        assert_eq!(d.on_exit, vec!["cmd1", "cmd2"]);
    }

    #[test]
    fn parse_env_with_multiple_equals() {
        let content = "#tendr: env=PATH=/usr/bin:/usr/local/bin\necho hi\n";
        let d = parse_directives(content).unwrap();
        // The full KEY=VALUE string is stored.
        assert_eq!(
            d.env.get("PATH").map(String::as_str),
            Some("PATH=/usr/bin:/usr/local/bin")
        );
    }

    #[test]
    fn parse_boolean_flags() {
        let content = "#tendr: replace\n#tendr: detach\n#tendr: stdin=pipe\necho hi\n";
        let d = parse_directives(content).unwrap();
        assert!(d.replace);
        assert!(d.detach);
        assert!(d.stdin_pipe);
    }

    #[test]
    fn parse_invalid_stdin_value() {
        let content = "#tendr: stdin=yes\necho hi\n";
        let err = parse_directives(content).unwrap_err();
        assert!(matches!(err, DirectiveError::InvalidStdin(_)));
    }

    #[test]
    fn parse_session_override() {
        let content = "#tendr: session=my-custom-name\necho hi\n";
        let d = parse_directives(content).unwrap();
        assert_eq!(d.session.as_deref(), Some("my-custom-name"));
    }

    #[test]
    fn parse_skips_regular_comments() {
        let content = "\
#!/bin/bash
# This is a regular comment
#tendr: namespace=test
# Another comment

echo hi
";
        let d = parse_directives(content).unwrap();
        assert_eq!(d.namespace.as_deref(), Some("test"));
    }

    #[test]
    fn parse_empty_content() {
        let d = parse_directives("").unwrap();
        assert_eq!(d, Directives::default());
    }

    #[test]
    fn parse_whitespace_trimmed() {
        let content = "#tendr: namespace= builds \necho hi\n";
        let d = parse_directives(content).unwrap();
        assert_eq!(d.namespace.as_deref(), Some("builds"));
    }

    // --- Name derivation tests ---

    #[test]
    fn derive_name_build_sh() {
        assert_eq!(derive_session_name(Path::new("build.sh")).unwrap(), "build");
    }

    #[test]
    fn derive_name_my_build_sh() {
        assert_eq!(
            derive_session_name(Path::new("my.build.sh")).unwrap(),
            "my-build"
        );
    }

    #[test]
    fn derive_name_private_sh() {
        assert_eq!(
            derive_session_name(Path::new("_private.sh")).unwrap(),
            "private"
        );
    }

    #[test]
    fn derive_name_makefile() {
        assert_eq!(
            derive_session_name(Path::new("Makefile")).unwrap(),
            "Makefile"
        );
    }

    #[test]
    fn derive_name_hidden_sh() {
        assert_eq!(
            derive_session_name(Path::new(".hidden.sh")).unwrap(),
            "hidden"
        );
    }

    #[test]
    fn derive_name_full_path() {
        assert_eq!(
            derive_session_name(Path::new("/home/user/scripts/build.sh")).unwrap(),
            "build"
        );
    }

    #[test]
    fn derive_name_long_truncated() {
        let long_name = format!("{}.sh", "a".repeat(300));
        let result = derive_session_name(Path::new(&long_name)).unwrap();
        assert!(result.len() <= 255);
    }

    #[test]
    fn derive_name_utf8_truncation_safe() {
        // 'é' is 2 bytes in UTF-8. Create a name that would truncate mid-char.
        let name = format!("{}.sh", "a".repeat(254) + "é"); // 254 + 2 = 256 bytes stem
        let result = derive_session_name(Path::new(&name));
        // Should truncate to 254 bytes (before the 'é') without panicking.
        assert!(result.is_ok());
        assert!(result.unwrap().len() <= 255);
    }

    #[test]
    fn derive_name_only_underscores_errors() {
        let err = derive_session_name(Path::new("___.sh")).unwrap_err();
        assert!(err.contains("session=NAME"));
    }

    #[test]
    fn parse_invalid_namespace_errors() {
        let content = "#tendr: namespace=bad name\necho hi\n";
        let err = parse_directives(content).unwrap_err();
        assert!(matches!(err, DirectiveError::InvalidNamespace(_)));
    }

    #[test]
    fn parse_namespace_with_dot_errors() {
        let content = "#tendr: namespace=foo.bar\necho hi\n";
        let err = parse_directives(content).unwrap_err();
        assert!(matches!(err, DirectiveError::InvalidNamespace(_)));
    }

    #[test]
    fn parse_invalid_session_name_errors() {
        let content = "#tendr: session=my.bad.name\necho hi\n";
        let err = parse_directives(content).unwrap_err();
        assert!(matches!(err, DirectiveError::InvalidSession(_)));
    }

    #[test]
    fn parse_session_with_whitespace_errors() {
        let content = "#tendr: session=has space\necho hi\n";
        let err = parse_directives(content).unwrap_err();
        assert!(matches!(err, DirectiveError::InvalidSession(_)));
    }
}
