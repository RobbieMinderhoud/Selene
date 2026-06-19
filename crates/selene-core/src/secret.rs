//! A credential wrapper that never leaks its contents through `Debug`,
//! `Display`, serialization, or logging, and zeroes its memory on drop.
//!
//! Passwords and tokens are kept out of [`ConnectionSpec`](crate::ConnectionSpec)
//! entirely — they travel as a `Secret`, are persisted only in the OS keychain,
//! and are never serialized into config files or log lines.

use zeroize::Zeroize;

/// An opaque secret string (password / token).
///
/// `Secret` deliberately implements neither `Display` nor `serde::Serialize`,
/// and its `Debug` output is redacted, so it cannot accidentally end up in a
/// log line, error message, or persisted config.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the raw secret. Call this only at the point of use (e.g. building
    /// a driver connection); never store, log, or format the result.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is empty (e.g. an unset password).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert!(!format!("{s:?}").contains("hunter2"));
    }

    #[test]
    fn expose_returns_value() {
        let s = Secret::new("hunter2");
        assert_eq!(s.expose(), "hunter2");
        assert!(!s.is_empty());
    }
}
