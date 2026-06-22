//! The IPC-facing error type.
//!
//! Every command returns `Result<T, IpcError>`. `IpcError` is a flat,
//! serializable shape so the frontend can branch on a stable `kind` string and
//! show the (already-sanitized) `message`.
//!
//! ## Secret safety
//! The `message` is derived from [`CoreError`](selene_core::CoreError)'s
//! `Display`, which the core constructs from Selene-owned text only — driver
//! errors are sanitized at the boundary in `selene-core` and
//! [`Secret`](selene_core::Secret) implements neither `Display` nor `Serialize`.
//! So an `IpcError` can never carry a password, token, or connection string.
//! Do not construct an `IpcError` from raw server text or a `Secret` here.

use serde::Serialize;

use selene_core::CoreError;

/// A flat, serializable error returned by every IPC command.
///
/// `kind` is a stable machine-readable discriminant (matching the
/// [`CoreError`] variant, plus a few IPC-only kinds); `message` is a
/// human-readable, secret-free description.
#[derive(Debug, Clone, Serialize)]
pub struct IpcError {
    /// Human-readable, sanitized description (never contains secrets).
    pub message: String,
    /// Stable machine-readable category (see [`IpcError`] docs for the set).
    pub kind: String,
}

impl IpcError {
    /// Build an `IpcError` from a `kind` discriminant and a (secret-free)
    /// message. Used for IPC-layer conditions that have no [`CoreError`]
    /// counterpart (e.g. an unknown session id or a guard block).
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }

    /// A reference to an unknown live session (the frontend passed a
    /// `session_id` that is not, or no longer, connected).
    pub fn unknown_session(session_id: &str) -> Self {
        Self::new(
            "unknown_session",
            format!("no live session for id '{session_id}'"),
        )
    }

    /// A reference to an unknown saved connection.
    pub fn unknown_connection(connection_id: &str) -> Self {
        Self::new(
            "unknown_connection",
            format!("no saved connection with id '{connection_id}'"),
        )
    }

    /// The SQL guard refused to run the batch (read-only / blocked statement).
    pub fn blocked(reasons: &[String]) -> Self {
        let detail = if reasons.is_empty() {
            "statement blocked by the SQL guard".to_string()
        } else {
            format!("statement blocked by the SQL guard: {}", reasons.join("; "))
        };
        Self::new("blocked", detail)
    }
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.kind)
    }
}

impl std::error::Error for IpcError {}

impl From<CoreError> for IpcError {
    fn from(err: CoreError) -> Self {
        // The `message` is `CoreError`'s own `Display`, which is built from
        // Selene-owned, sanitized text only — safe to forward verbatim.
        let kind = match &err {
            CoreError::Connection(_) => "connection",
            CoreError::Tls(_) => "tls",
            CoreError::Query(_) => "query",
            CoreError::Introspection(_) => "introspection",
            CoreError::Export(_) => "export",
            CoreError::Import(_) => "import",
            CoreError::Secret(_) => "secret",
            CoreError::Config(_) => "config",
            CoreError::Unsupported(_) => "unsupported",
            // Recoverable on the frontend: it can offer a forced retry that
            // disconnects the active sessions.
            CoreError::DatabaseInUse(_) => "database_in_use",
            CoreError::Cancelled => "cancelled",
            CoreError::Protocol(_) => "protocol",
            CoreError::Io(_) => "io",
            // `CoreError` is `#[non_exhaustive]`: map any future variant to a
            // generic kind rather than failing to compile or panicking.
            _ => "core",
        };
        Self {
            kind: kind.to_string(),
            message: err.to_string(),
        }
    }
}
