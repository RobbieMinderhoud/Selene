//! Builds [`PgConnectOptions`] from Selene's [`ConnectionSpec`] + [`Secret`].
//!
//! This is the single place where the secret is exposed: it is read with
//! [`Secret::expose`] only to hand it to `.password(…)`, and is never stored,
//! logged, or `Debug`-printed by Selene.
//!
//! ⚠️ Unlike tiberius' `Config`, sqlx's [`PgConnectOptions`] does **not** redact
//! the password in its `Debug` impl — it prints `password: Some("…")` verbatim.
//! So the options value, once built, must never be logged or `Debug`-formatted.
//! The driver only ever calls `.connect()` on it (see the parent module); the
//! secret stays inside the options and out of every log line.

use sqlx::postgres::{PgConnectOptions, PgSslMode};

use crate::connection_spec::{AuthMethod as SpecAuth, ConnectionSpec};
use crate::error::CoreError;
use crate::secret::Secret;

/// Translate a [`ConnectionSpec`] and its [`Secret`] into [`PgConnectOptions`].
///
/// Postgres always requires a login, so [`AuthMethod::SqlLogin`](SpecAuth::SqlLogin)
/// is the only accepted method; [`AuthMethod::None`](SpecAuth::None) is rejected
/// with a [`CoreError::Config`].
///
/// TLS: `spec.tls.encrypt` maps to [`PgSslMode::Require`] (else
/// [`PgSslMode::Prefer`], libpq's default — opportunistic TLS that silently
/// falls back to plaintext). `trust_server_certificate` keeps `Require` (which
/// encrypts but does **not** validate the certificate chain), deliberately *not*
/// `VerifyCa`/`VerifyFull`: the toggle exists to accept a self-signed dev-server
/// certificate, so chain verification must stay off.
pub(crate) fn build_options(
    spec: &ConnectionSpec,
    secret: &Secret,
) -> Result<PgConnectOptions, CoreError> {
    let host = spec.host.trim();
    if host.is_empty() {
        return Err(CoreError::Config("host must not be empty".into()));
    }

    let mut options = PgConnectOptions::new()
        .host(host)
        .port(spec.effective_port().unwrap_or(5432));

    // Authentication. Only SQL logins are modelled today. `AuthMethod` is
    // `#[non_exhaustive]` for downstream crates, but within `selene-core` the
    // match is exhaustive; a future variant (Integrated, Entra ID, …) will force
    // an arm here.
    match &spec.auth {
        SpecAuth::SqlLogin { username } => {
            if username.trim().is_empty() {
                return Err(CoreError::Config("username must not be empty".into()));
            }
            options = options.username(username);
            // The ONLY point the secret is exposed — handed straight to sqlx.
            options = options.password(secret.expose());
        }
        // `AuthMethod::None` exists for password-less backends (SQLite); Postgres
        // always needs a login, so reject it rather than attempt an anonymous
        // connect that would fail less clearly downstream.
        SpecAuth::None => {
            return Err(CoreError::Config("PostgreSQL requires a login".into()));
        }
        // SCRAM (as modelled here) is a MongoDB auth method; reject it.
        SpecAuth::ScramLogin { .. } => {
            return Err(CoreError::Config(
                "PostgreSQL does not support SCRAM auth".into(),
            ));
        }
    }

    if let Some(database) = spec.database.as_deref() {
        if !database.is_empty() {
            options = options.database(database);
        }
    }

    // Transport security. `Require` encrypts the connection; with
    // `trust_server_certificate` it still does not verify the chain (accepts a
    // self-signed cert). `Prefer` is libpq's opportunistic default.
    let ssl_mode = if spec.tls.encrypt {
        PgSslMode::Require
    } else {
        PgSslMode::Prefer
    };
    options = options.ssl_mode(ssl_mode);

    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_spec::{AuthMethod, DriverId, TlsConfig};

    fn spec() -> ConnectionSpec {
        ConnectionSpec {
            id: "p1".into(),
            name: "Test".into(),
            driver: DriverId::Postgres,
            host: "db.example.invalid".into(),
            port: None,
            instance: None,
            uri: None,
            database: Some("appdb".into()),
            auth: AuthMethod::SqlLogin {
                username: "selene".into(),
            },
            tls: TlsConfig::default(),
            read_only: false,
        }
    }

    #[test]
    fn builds_from_a_spec() {
        let opts = build_options(&spec(), &Secret::new("pw")).expect("options build");
        assert_eq!(opts.get_host(), "db.example.invalid");
        // No explicit port => the Postgres default.
        assert_eq!(opts.get_port(), 5432);
    }

    #[test]
    fn explicit_port_overrides_default() {
        let mut s = spec();
        s.port = Some(54330);
        let opts = build_options(&s, &Secret::new("pw")).expect("options build");
        assert_eq!(opts.get_port(), 54330);
    }

    #[test]
    fn empty_host_is_rejected() {
        let mut s = spec();
        s.host = "   ".into();
        let err = build_options(&s, &Secret::new("pw")).unwrap_err();
        assert!(matches!(err, CoreError::Config(_)));
    }

    #[test]
    fn empty_username_is_rejected() {
        let mut s = spec();
        s.auth = AuthMethod::SqlLogin {
            username: "  ".into(),
        };
        let err = build_options(&s, &Secret::new("pw")).unwrap_err();
        assert!(matches!(err, CoreError::Config(_)));
    }

    #[test]
    fn auth_none_is_rejected() {
        let mut s = spec();
        s.auth = AuthMethod::None;
        let err = build_options(&s, &Secret::new("pw")).unwrap_err();
        assert!(matches!(err, CoreError::Config(_)));
    }

    // NOTE: there is deliberately no "password not in Debug" test here. sqlx's
    // `PgConnectOptions` Debug prints the password verbatim (`password: Some(…)`),
    // so such a test would (correctly) fail. The driver's contract is instead
    // that the options value is *never* Debug-formatted or logged — it is only
    // ever handed to `.connect()`. The `Secret` newtype keeps the password
    // redacted everywhere Selene itself owns it; the module docs flag the sqlx
    // Debug hazard so future edits do not log the options.
}
