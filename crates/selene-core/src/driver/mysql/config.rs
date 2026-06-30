//! Builds [`MySqlConnectOptions`] from Selene's [`ConnectionSpec`] + [`Secret`].
//!
//! This is the single place where the secret is exposed: it is read with
//! [`Secret::expose`] only to hand it to `.password(…)`, and is never stored,
//! logged, or `Debug`-printed by Selene.
//!
//! ⚠️ Exactly like sqlx's `PgConnectOptions`, [`MySqlConnectOptions`] does **not**
//! redact the password in its `Debug` impl. So the options value, once built,
//! must never be logged or `Debug`-formatted. The driver only ever calls
//! `.connect()` on it (see the parent module); the secret stays inside the
//! options and out of every log line.

use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};

use crate::connection_spec::{AuthMethod as SpecAuth, ConnectionSpec};
use crate::error::CoreError;
use crate::secret::Secret;

/// Translate a [`ConnectionSpec`] and its [`Secret`] into [`MySqlConnectOptions`].
///
/// MySQL always requires a login, so [`AuthMethod::SqlLogin`](SpecAuth::SqlLogin)
/// is the only accepted method; [`AuthMethod::None`](SpecAuth::None) is rejected
/// with a [`CoreError::Config`].
///
/// TLS: `spec.tls.encrypt` maps to [`MySqlSslMode::Required`] (else
/// [`MySqlSslMode::Preferred`], MySQL's opportunistic default that silently falls
/// back to plaintext). `trust_server_certificate` keeps `Required` (which
/// encrypts but does **not** validate the certificate chain), deliberately *not*
/// `VerifyCa`/`VerifyIdentity`: the toggle exists to accept a self-signed
/// dev-server certificate, so chain verification must stay off.
pub(crate) fn build_options(
    spec: &ConnectionSpec,
    secret: &Secret,
) -> Result<MySqlConnectOptions, CoreError> {
    let host = spec.host.trim();
    if host.is_empty() {
        return Err(CoreError::Config("host must not be empty".into()));
    }

    let mut options = MySqlConnectOptions::new()
        .host(host)
        .port(spec.effective_port().unwrap_or(3306));

    // Authentication. Only SQL logins are modelled today. `AuthMethod` is
    // `#[non_exhaustive]` for downstream crates, but within `selene-core` the
    // match is exhaustive; a future variant will force an arm here.
    match &spec.auth {
        SpecAuth::SqlLogin { username } => {
            if username.trim().is_empty() {
                return Err(CoreError::Config("username must not be empty".into()));
            }
            options = options.username(username);
            // The ONLY point the secret is exposed — handed straight to sqlx.
            // An empty password is permitted (some dev servers allow it); only an
            // empty *username* is rejected above.
            options = options.password(secret.expose());
        }
        // `AuthMethod::None` exists for password-less backends (SQLite); MySQL
        // always needs a login, so reject it rather than attempt an anonymous
        // connect that would fail less clearly downstream.
        SpecAuth::None => {
            return Err(CoreError::Config("MySQL requires a login".into()));
        }
    }

    if let Some(database) = spec.database.as_deref() {
        if !database.is_empty() {
            options = options.database(database);
        }
    }

    // Transport security. `Required` encrypts the connection; with
    // `trust_server_certificate` it still does not verify the chain (accepts a
    // self-signed cert) — we deliberately do NOT use `VerifyCa`/`VerifyIdentity`.
    // `Preferred` is MySQL's opportunistic default.
    let ssl_mode = if spec.tls.encrypt {
        MySqlSslMode::Required
    } else {
        MySqlSslMode::Preferred
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
            id: "m1".into(),
            name: "Test".into(),
            driver: DriverId::Mysql,
            host: "db.example.invalid".into(),
            port: None,
            instance: None,
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
        // No explicit port => the MySQL default.
        assert_eq!(opts.get_port(), 3306);
    }

    #[test]
    fn explicit_port_overrides_default() {
        let mut s = spec();
        s.port = Some(33060);
        let opts = build_options(&s, &Secret::new("pw")).expect("options build");
        assert_eq!(opts.get_port(), 33060);
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

    // NOTE: there is deliberately no "password not in Debug" test here. Like
    // `PgConnectOptions`, sqlx's `MySqlConnectOptions` Debug is NOT redaction-safe
    // (it prints the password), so such a test would (correctly) fail. The
    // driver's contract is instead that the options value is *never* Debug-
    // formatted or logged — it is only ever handed to `.connect()`. The `Secret`
    // newtype keeps the password redacted everywhere Selene itself owns it; the
    // module docs flag the sqlx Debug hazard so future edits do not log it.
}
