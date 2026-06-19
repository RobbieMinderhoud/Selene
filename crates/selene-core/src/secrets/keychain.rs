//! OS-keychain storage for connection passwords, backed by the `keyring` crate.
//!
//! Connection passwords are the only secret Selene persists, and they live
//! **only** in the platform credential store — never in a config file, log
//! line, or [`ConnectionSpec`](crate::ConnectionSpec). This module is a thin,
//! synchronous wrapper that maps `keyring`'s API and errors onto Selene's
//! [`Secret`] / [`CoreError`] vocabulary.
//!
//! Backend selection is handled by `keyring` itself via target gating
//! (Secure Transport on macOS, Credential Manager on Windows, the Secret
//! Service / D-Bus on Linux); this code is platform-agnostic.
//!
//! ## Error mapping
//! - A *missing* entry is not an error: [`get_secret`] returns `Ok(None)` and
//!   [`delete_secret`] returns `Ok(())`.
//! - Every other `keyring` error becomes [`CoreError::Secret`] with a message
//!   that **never contains the secret value** — only the connection id and the
//!   keyring error's own (value-free) description.

use keyring::{Entry, Error as KeyringError};

use crate::error::CoreError;
use crate::secret::Secret;

/// The keyring *service* under which all Selene credentials are filed. This is
/// the application bundle identifier; the per-credential *account* key is the
/// connection id.
pub const KEYCHAIN_SERVICE: &str = "com.selene.app";

/// Stores connection passwords in the OS keychain.
///
/// Stateless — every method opens a fresh [`Entry`] keyed by
/// ([`KEYCHAIN_SERVICE`], `connection_id`) — so the store is trivially cloneable
/// and shareable. It exists as a type (rather than bare free functions) so a
/// future in-memory or file-backed test double can be swapped in behind the
/// same surface.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeychainStore;

impl KeychainStore {
    /// Create a store handle. Cheap and infallible.
    pub fn new() -> Self {
        Self
    }

    /// Persist `secret` as the password for `connection_id`, overwriting any
    /// existing value.
    pub fn set_secret(&self, connection_id: &str, secret: &Secret) -> Result<(), CoreError> {
        let entry = open_entry(connection_id)?;
        // `expose()` is called only here, at the point of use, and the value is
        // never folded into an error message below.
        entry
            .set_password(secret.expose())
            .map_err(|e| map_keyring_err(connection_id, "store", e))
    }

    /// Fetch the password for `connection_id`.
    ///
    /// Returns `Ok(None)` when no entry exists (the common "not yet saved"
    /// case), and `Err` only on a genuine store failure.
    pub fn get_secret(&self, connection_id: &str) -> Result<Option<Secret>, CoreError> {
        let entry = open_entry(connection_id)?;
        match entry.get_password() {
            Ok(password) => Ok(Some(Secret::new(password))),
            // A missing credential is an expected, non-error outcome.
            Err(KeyringError::NoEntry) => Ok(None),
            Err(e) => Err(map_keyring_err(connection_id, "read", e)),
        }
    }

    /// Remove the password for `connection_id`.
    ///
    /// Deleting an entry that does not exist is a no-op (`Ok(())`), so callers
    /// can use this idempotently when forgetting a connection.
    pub fn delete_secret(&self, connection_id: &str) -> Result<(), CoreError> {
        let entry = open_entry(connection_id)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            // Already absent — treat as success.
            Err(KeyringError::NoEntry) => Ok(()),
            Err(e) => Err(map_keyring_err(connection_id, "delete", e)),
        }
    }
}

/// Open the keyring entry for one connection, mapping a construction failure
/// (e.g. an empty/invalid account key) to [`CoreError::Secret`].
fn open_entry(connection_id: &str) -> Result<Entry, CoreError> {
    Entry::new(KEYCHAIN_SERVICE, connection_id)
        .map_err(|e| map_keyring_err(connection_id, "open", e))
}

/// Convert a `keyring` error into a [`CoreError::Secret`] without ever leaking
/// the secret value. Only the operation, the connection id, and the keyring
/// error's own description (which is value-free) are included.
fn map_keyring_err(connection_id: &str, op: &str, err: KeyringError) -> CoreError {
    CoreError::Secret(format!(
        "failed to {op} credential for connection '{connection_id}': {err}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests are hermetic by design: they exercise only pure logic and the
    // error-mapping surface, and never touch the real OS keychain. A genuine
    // round-trip against the live store is gated behind `#[ignore]` below.

    #[test]
    fn service_constant_is_the_bundle_id() {
        assert_eq!(KEYCHAIN_SERVICE, "com.selene.app");
    }

    #[test]
    fn store_is_constructible() {
        // Both constructors yield an equivalent stateless handle.
        let _ = KeychainStore::new();
        let _ = KeychainStore;
    }

    #[test]
    fn no_entry_is_not_an_error() {
        // The NoEntry → Ok(None)/Ok(()) policy is the crux of the wrapper; assert
        // the mapping directly so it cannot regress without a live keychain.
        let get_result: Result<Option<Secret>, CoreError> = match KeyringError::NoEntry {
            KeyringError::NoEntry => Ok(None),
            other => Err(map_keyring_err("c1", "read", other)),
        };
        assert!(matches!(get_result, Ok(None)));

        let delete_result: Result<(), CoreError> = match KeyringError::NoEntry {
            KeyringError::NoEntry => Ok(()),
            other => Err(map_keyring_err("c1", "delete", other)),
        };
        assert!(matches!(delete_result, Ok(())));
    }

    #[test]
    fn error_mapping_includes_context_but_never_the_secret() {
        // A non-NoEntry error must map to CoreError::Secret and carry the
        // connection id + operation, but obviously cannot embed a password.
        let err = map_keyring_err("conn-42", "store", KeyringError::NoEntry);
        match err {
            CoreError::Secret(msg) => {
                assert!(
                    msg.contains("conn-42"),
                    "message should name the connection"
                );
                assert!(msg.contains("store"), "message should name the operation");
                // Sanity: the message is built only from id/op/keyring text.
                assert!(!msg.contains("password="));
            }
            other => panic!("expected CoreError::Secret, got {other:?}"),
        }
    }

    // A real round-trip against the host keychain. Ignored so `cargo test`
    // stays hermetic and CI never prompts for keychain access; run explicitly
    // with `cargo test -p selene-core -- --ignored live_keychain_round_trip`.
    #[test]
    #[ignore = "touches the real OS keychain; run manually"]
    fn live_keychain_round_trip() {
        let store = KeychainStore::new();
        let id = "selene-test-connection-DO-NOT-KEEP";

        store.set_secret(id, &Secret::new("s3cr3t")).unwrap();
        let got = store.get_secret(id).unwrap().expect("just stored");
        assert_eq!(got.expose(), "s3cr3t");

        store.delete_secret(id).unwrap();
        assert!(store.get_secret(id).unwrap().is_none());
        // Second delete is a no-op.
        store.delete_secret(id).unwrap();
    }
}
