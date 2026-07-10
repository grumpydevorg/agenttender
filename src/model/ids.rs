use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::num::NonZeroU32;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Globally unique execution identity. UUID v7 (time-sortable).
/// The authoritative identifier for a run — all lifecycle decisions use this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(uuid::Uuid);

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl RunId {
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// The inner UUID (always v7). Used to derive event writer identity.
    #[must_use]
    pub fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }

    #[cfg(test)]
    pub fn from_uuid(uuid: uuid::Uuid) -> Self {
        Self(uuid)
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for RunId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let uuid = uuid::Uuid::deserialize(deserializer)?;
        if uuid.get_version_num() != 7 {
            return Err(serde::de::Error::custom(format!(
                "expected UUID v7, got v{}",
                uuid.get_version_num()
            )));
        }
        Ok(Self(uuid))
    }
}

/// Monotonic counter per session name. Starts at 1, never zero.
/// Human-readable, useful for debugging. NOT used for lifecycle decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation(u64);

impl Generation {
    #[must_use]
    pub fn first() -> Self {
        Self(1)
    }

    /// Construct from a raw value. Returns first() if n is 0.
    #[must_use]
    pub fn from_u64(n: u64) -> Self {
        if n == 0 { Self::first() } else { Self(n) }
    }

    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for Generation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for Generation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u64::deserialize(deserializer)?;
        if value == 0 {
            return Err(serde::de::Error::custom("generation cannot be zero"));
        }
        Ok(Self(value))
    }
}

/// Validated session name. Non-empty, no slashes, no dots, no whitespace,
/// no underscore prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionName(String);

#[derive(Debug, Error)]
pub enum SessionNameError {
    #[error("session name cannot be empty")]
    Empty,
    #[error("session name cannot contain '/'")]
    ContainsSlash,
    #[error("session name cannot contain '.'")]
    ContainsDot,
    #[error("session name cannot contain whitespace")]
    ContainsWhitespace,
    #[error("session name cannot start with '_'")]
    StartsWithUnderscore,

    #[error("session name too long (max {MAX_SESSION_NAME_LEN} bytes)")]
    TooLong,
}

/// Maximum length for a session name. Matches the POSIX filename component limit
/// and leaves room for filenames like `meta.json.tmp` under the session directory.
const MAX_SESSION_NAME_LEN: usize = 255;

impl SessionName {
    /// Create a new validated session name.
    ///
    /// # Errors
    /// Returns `SessionNameError` if the name is empty, contains invalid
    /// characters, or starts with an underscore.
    pub fn new(name: &str) -> Result<Self, SessionNameError> {
        Self::validate(name)?;
        Ok(Self(name.to_owned()))
    }

    fn validate(name: &str) -> Result<(), SessionNameError> {
        if name.is_empty() {
            return Err(SessionNameError::Empty);
        }
        if name.len() > MAX_SESSION_NAME_LEN {
            return Err(SessionNameError::TooLong);
        }
        if name.contains('/') {
            return Err(SessionNameError::ContainsSlash);
        }
        if name.contains('.') {
            return Err(SessionNameError::ContainsDot);
        }
        if name.chars().any(|c| c.is_whitespace()) {
            return Err(SessionNameError::ContainsWhitespace);
        }
        if name.starts_with('_') {
            return Err(SessionNameError::StartsWithUnderscore);
        }
        Ok(())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for SessionName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SessionName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::validate(&s).map_err(serde::de::Error::custom)?;
        Ok(Self(s))
    }
}

/// Validated namespace for grouping sessions. Same validation rules as SessionName.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Namespace(String);

#[derive(Debug, Error)]
pub enum NamespaceError {
    #[error("namespace cannot be empty")]
    Empty,
    #[error("namespace cannot contain '/'")]
    ContainsSlash,
    #[error("namespace cannot contain '.'")]
    ContainsDot,
    #[error("namespace cannot contain whitespace")]
    ContainsWhitespace,
    #[error("namespace cannot start with '_'")]
    StartsWithUnderscore,
    #[error("namespace too long (max {MAX_NAMESPACE_LEN} bytes)")]
    TooLong,
}

const MAX_NAMESPACE_LEN: usize = 255;

impl Namespace {
    /// Create a new validated namespace.
    ///
    /// # Errors
    /// Returns `NamespaceError` if the name is empty, contains invalid
    /// characters, or starts with an underscore.
    pub fn new(name: &str) -> Result<Self, NamespaceError> {
        Self::validate(name)?;
        Ok(Self(name.to_owned()))
    }

    fn validate(name: &str) -> Result<(), NamespaceError> {
        if name.is_empty() {
            return Err(NamespaceError::Empty);
        }
        if name.len() > MAX_NAMESPACE_LEN {
            return Err(NamespaceError::TooLong);
        }
        if name.contains('/') {
            return Err(NamespaceError::ContainsSlash);
        }
        if name.contains('.') {
            return Err(NamespaceError::ContainsDot);
        }
        if name.chars().any(|c| c.is_whitespace()) {
            return Err(NamespaceError::ContainsWhitespace);
        }
        if name.starts_with('_') {
            return Err(NamespaceError::StartsWithUnderscore);
        }
        Ok(())
    }

    /// Returns the default namespace ("default").
    #[must_use]
    pub fn default_namespace() -> Self {
        Self("default".to_owned())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for Namespace {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Namespace {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::validate(&s).map_err(serde::de::Error::custom)?;
        Ok(Self(s))
    }
}

/// Validated annotation source. Dotted string identifying who produced an annotation.
/// `tender.*` is reserved for internal use.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Source(String);

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("source cannot be empty")]
    Empty,
    #[error("source must contain at least one '.'")]
    NoDot,
    #[error("source contains invalid character '{0}' (allowed: ASCII alphanumeric, '.', '-')")]
    InvalidChar(char),
    #[error("source cannot start with 'tender.' (reserved)")]
    ReservedPrefix,
    #[error("source has empty segment (leading, trailing, or consecutive dots)")]
    EmptySegment,
    #[error("source too long (max {MAX_SOURCE_LEN} bytes)")]
    TooLong,
}

const MAX_SOURCE_LEN: usize = 128;

impl Source {
    /// Create a new validated source from user input.
    ///
    /// # Errors
    /// Returns `SourceError` if the source is empty, missing a dot,
    /// contains invalid characters, or uses the reserved `tender.*` prefix.
    pub fn new(s: &str) -> Result<Self, SourceError> {
        Self::validate_grammar(s)?;
        if s.starts_with("tender.") {
            return Err(SourceError::ReservedPrefix);
        }
        Ok(Self(s.to_owned()))
    }

    /// Create a source at a tender-internal call site. Grammar is still
    /// enforced, but the `tender.*` reservation is not — internal writers
    /// (`tender.sidecar`, `tender.exec`, `tender.cli`) are the only
    /// legitimate users of the reserved prefix.
    ///
    /// # Errors
    /// Returns `SourceError` on grammar violations.
    pub fn trusted(s: &str) -> Result<Self, SourceError> {
        Self::validate_grammar(s)?;
        Ok(Self(s.to_owned()))
    }

    fn validate_grammar(s: &str) -> Result<(), SourceError> {
        if s.is_empty() {
            return Err(SourceError::Empty);
        }
        if s.len() > MAX_SOURCE_LEN {
            return Err(SourceError::TooLong);
        }
        if let Some(c) = s
            .chars()
            .find(|c| !c.is_ascii_alphanumeric() && *c != '.' && *c != '-')
        {
            return Err(SourceError::InvalidChar(c));
        }
        if !s.contains('.') {
            return Err(SourceError::NoDot);
        }
        if s.starts_with('.') || s.ends_with('.') || s.contains("..") {
            return Err(SourceError::EmptySegment);
        }
        Ok(())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for Source {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Grammar only: the `tender.*` reservation is a user-input-boundary
        // rule (`Source::new`). Stored records legitimately carry internal
        // sources like `tender.sidecar` and must round-trip.
        let s = String::deserialize(deserializer)?;
        Self::validate_grammar(&s).map_err(serde::de::Error::custom)?;
        Ok(Self(s))
    }
}

/// Identity of a running process. PID alone is unsafe due to reuse —
/// always pair with birth time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: NonZeroU32,
    pub start_time_ns: u64,
}

impl ProcessIdentity {
    /// Identity for a child that spawned successfully but had already exited
    /// before its start time could be read (a fast-exit race). The `0` start
    /// time is a sentinel that can never equal a live process's real start
    /// time, so this identity only ever compares as `Missing`/`IdentityMismatch`
    /// — never as a false "alive" — which keeps PID-reuse-safe kill correct.
    #[must_use]
    pub fn already_exited(pid: NonZeroU32) -> Self {
        Self {
            pid,
            start_time_ns: 0,
        }
    }
}

/// Validated epoch timestamp in seconds. Serializes as string for schema v1
/// compatibility. Accepts both string ("1773653954") and integer (1773653954)
/// on deserialization for backwards compatibility with existing meta.json files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochTimestamp(u64);

impl EpochTimestamp {
    #[must_use]
    pub fn now() -> Self {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self(secs)
    }

    #[must_use]
    pub fn from_secs(secs: u64) -> Self {
        Self(secs)
    }

    #[must_use]
    pub fn as_secs(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for EpochTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for EpochTimestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Always serialize as string for JSON compatibility with schema v1
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for EpochTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TimestampVisitor;

        impl<'de> serde::de::Visitor<'de> for TimestampVisitor {
            type Value = EpochTimestamp;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("epoch seconds as string or integer")
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<EpochTimestamp, E> {
                Ok(EpochTimestamp(v))
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<EpochTimestamp, E> {
                v.parse::<u64>()
                    .map(EpochTimestamp)
                    .map_err(|_| E::custom(format!("invalid epoch timestamp: {v}")))
            }
        }

        deserializer.deserialize_any(TimestampVisitor)
    }
}
