//! Builds [`SqliteConnectOptions`] from Selene's [`ConnectionSpec`].
//!
//! SQLite has no host/port/auth: the "connection" is just a file path. We carry
//! that path in [`ConnectionSpec::host`] (the field the connection sidebar
//! already populates) and ignore `port`, `database`, `auth`, and `tls`.
//!
//! `create_if_missing(false)` is deliberate: opening a path that does not exist
//! is an *error*, not a silent "create an empty database". A SQL editor's
//! "connect" should fail clearly on a typo'd path rather than conjure a stray
//! empty `.db`.

use sqlx::sqlite::SqliteConnectOptions;

use crate::connection_spec::ConnectionSpec;
use crate::error::CoreError;

/// Translate a [`ConnectionSpec`] into [`SqliteConnectOptions`].
///
/// The database file path comes from `spec.host`; an empty path is rejected.
pub(crate) fn build_options(spec: &ConnectionSpec) -> Result<SqliteConnectOptions, CoreError> {
    let path = spec.host.trim();
    if path.is_empty() {
        return Err(CoreError::Config(
            "database file path must not be empty".into(),
        ));
    }

    // `.filename` takes the path verbatim; `.create_if_missing(false)` makes a
    // missing file an error rather than creating one.
    Ok(SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_spec::{AuthMethod, DriverId, TlsConfig};

    fn spec_with_host(host: &str) -> ConnectionSpec {
        ConnectionSpec {
            id: "s1".into(),
            name: "Local".into(),
            driver: DriverId::Sqlite,
            host: host.into(),
            port: None,
            instance: None,
            database: None,
            auth: AuthMethod::None,
            tls: TlsConfig::default(),
            read_only: false,
        }
    }

    #[test]
    fn builds_from_a_file_path() {
        let opts = build_options(&spec_with_host("/tmp/app.db")).expect("options build");
        // The filename round-trips through the options' Debug.
        assert!(format!("{opts:?}").contains("app.db"));
    }

    #[test]
    fn empty_path_is_rejected() {
        let err = build_options(&spec_with_host("   ")).unwrap_err();
        assert!(matches!(err, CoreError::Config(_)));
    }
}
