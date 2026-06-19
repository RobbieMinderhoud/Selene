//! The crate-wide error type.
//!
//! IMPORTANT: error messages are constructed by Selene and must **never** embed
//! secrets (passwords, tokens, connection strings). Driver implementations map
//! backend errors into these variants and are responsible for sanitizing any
//! server-provided text before it reaches a message.

/// Errors produced by the data layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoreError {
    /// Connecting to or authenticating against the server failed.
    #[error("connection failed: {0}")]
    Connection(String),

    /// A TLS / encryption negotiation problem.
    #[error("TLS error: {0}")]
    Tls(String),

    /// A query failed to execute (syntax, permissions, runtime error).
    #[error("query failed: {0}")]
    Query(String),

    /// Schema introspection failed.
    #[error("introspection failed: {0}")]
    Introspection(String),

    /// Exporting a result set failed (formatting or I/O).
    #[error("export failed: {0}")]
    Export(String),

    /// Importing data failed (CSV parsing, type coercion, or the insert).
    #[error("import failed: {0}")]
    Import(String),

    /// The OS keychain / secret store returned an error.
    #[error("secret store error: {0}")]
    Secret(String),

    /// The connection specification is invalid or incomplete.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// The requested operation is not supported by this driver/build.
    #[error("operation not supported: {0}")]
    Unsupported(String),

    /// The operation was cancelled by the caller.
    #[error("operation was cancelled")]
    Cancelled,

    /// An unexpected protocol or driver-internal error.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// A low-level I/O error (already stringified to avoid leaking details).
    #[error("I/O error: {0}")]
    Io(String),
}
