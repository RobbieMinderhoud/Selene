//! Builds [`mongodb::options::ClientOptions`] from Selene's [`ConnectionSpec`]
//! and a [`Secret`].
//!
//! Two shapes are supported:
//! - **URI**: if `spec.uri` is set, parse it (`mongodb://` / `mongodb+srv://`,
//!   which covers replica sets, auth, and TLS). Discrete host/port/auth fields
//!   are treated as overlays — we only supply a credential when the URI itself
//!   did not, so a URI-embedded password always wins.
//! - **Discrete**: otherwise build from `spec.host` + `spec.effective_port()`
//!   and an optional credential derived from the auth method.
//!
//! Security: the password is exposed via [`Secret::expose`] at exactly one point
//! (building the `Credential`). Like sqlx's `PgConnectOptions`, the mongodb
//! driver's `Credential`/`ClientOptions` Debug print the password verbatim, so
//! the built options are **never** logged or Debug-formatted — they are only
//! handed to `Client::with_options`. The `Secret` newtype keeps the password
//! redacted everywhere Selene itself owns it.

use std::str::FromStr;
use std::time::Duration;

use mongodb::options::{AuthMechanism, ClientOptions, Credential, ServerAddress, Tls, TlsOptions};

use crate::connection_spec::{AuthMethod as SpecAuth, ConnectionSpec};
use crate::error::CoreError;
use crate::secret::Secret;

use super::error::map_connect_err;

/// App name reported to the server (shows up in `db.currentOp()` / server logs).
const APP_NAME: &str = "Selene";

/// Upper bound on the initial connect and per-operation server selection. Mirrors
/// the mssql driver's 15s connect timeout intent: a dead/unreachable host should
/// fail promptly rather than hang for the driver's much longer defaults.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Build [`ClientOptions`] from a spec + secret.
pub(crate) async fn build_options(
    spec: &ConnectionSpec,
    secret: &Secret,
) -> Result<ClientOptions, CoreError> {
    let uri = spec.uri.as_deref().map(str::trim).unwrap_or("");

    let mut options = if !uri.is_empty() {
        // Parse the full connection string. This performs `+srv` SRV/TXT lookups
        // and resolves TLS/auth/replica-set options from the URI.
        let mut opts = ClientOptions::parse(uri).await.map_err(map_connect_err)?;
        // Overlay a credential ONLY if the URI didn't specify one and the spec
        // carries a SCRAM username — a URI-embedded credential always wins.
        if opts.credential.is_none() {
            if let Some(cred) = credential_from_auth(&spec.auth, secret)? {
                opts.credential = Some(cred);
            }
        }
        opts
    } else {
        let host = spec.host.trim();
        if host.is_empty() {
            return Err(CoreError::Config(
                "MongoDB requires a host or a connection URI".into(),
            ));
        }
        let address = ServerAddress::Tcp {
            host: host.to_string(),
            port: Some(spec.effective_port().unwrap_or(27017)),
        };
        let mut opts = ClientOptions::builder().hosts(vec![address]).build();
        opts.credential = credential_from_auth(&spec.auth, secret)?;

        // TLS is only applied on the discrete path; the URI path carries its own
        // `tls=`/`tlsAllowInvalidCertificates=` settings.
        if spec.tls.encrypt {
            let tls = TlsOptions::builder()
                .allow_invalid_certificates(if spec.tls.trust_server_certificate {
                    Some(true)
                } else {
                    None
                })
                .build();
            opts.tls = Some(Tls::Enabled(tls));
        }
        opts
    };

    // Common settings on both paths.
    options.app_name = Some(APP_NAME.to_string());
    options.connect_timeout = Some(CONNECT_TIMEOUT);
    options.server_selection_timeout = Some(CONNECT_TIMEOUT);

    Ok(options)
}

/// Derive an optional [`Credential`] from the auth method.
///
/// - `ScramLogin` → SCRAM credential (source defaults to `admin`; mechanism
///   parsed when provided).
/// - `SqlLogin` → treat the username pragmatically as a SCRAM username with the
///   secret as the password, so an MSSQL-style spec still connects.
/// - `None` → no credential (anonymous connect).
fn credential_from_auth(auth: &SpecAuth, secret: &Secret) -> Result<Option<Credential>, CoreError> {
    match auth {
        SpecAuth::ScramLogin {
            username,
            auth_source,
            mechanism,
        } => {
            if username.trim().is_empty() {
                return Err(CoreError::Config("username must not be empty".into()));
            }
            // Parse an explicit mechanism (e.g. "SCRAM-SHA-256") when supplied;
            // otherwise leave it None so the driver negotiates with the server.
            let mechanism = match mechanism.as_deref() {
                Some(m) if !m.trim().is_empty() => Some(
                    AuthMechanism::from_str(m.trim())
                        .map_err(|e| CoreError::Config(e.to_string()))?,
                ),
                _ => None,
            };
            // SCRAM source defaults to "admin" when unset.
            let source = auth_source
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("admin");
            Ok(Some(build_credential(username, source, secret, mechanism)))
        }
        // Pragmatic: accept an MSSQL-style SQL login as a SCRAM username so a
        // spec authored for another backend still works against MongoDB.
        SpecAuth::SqlLogin { username } => {
            if username.trim().is_empty() {
                return Err(CoreError::Config("username must not be empty".into()));
            }
            Ok(Some(build_credential(username, "admin", secret, None)))
        }
        // Anonymous connect (e.g. a local dev server with auth disabled).
        SpecAuth::None => Ok(None),
    }
}

/// Assemble a [`Credential`]. This is the ONLY point the secret is exposed — it
/// is handed straight to the driver and the resulting value is never logged.
fn build_credential(
    username: &str,
    source: &str,
    secret: &Secret,
    mechanism: Option<AuthMechanism>,
) -> Credential {
    Credential::builder()
        .username(username.to_string())
        .source(source.to_string())
        .password(secret.expose().to_string())
        .mechanism(mechanism)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_spec::{DriverId, TlsConfig};

    fn spec() -> ConnectionSpec {
        ConnectionSpec {
            id: "mg1".into(),
            name: "Test".into(),
            driver: DriverId::Mongodb,
            host: "db.example.invalid".into(),
            port: None,
            instance: None,
            uri: None,
            database: Some("appdb".into()),
            auth: SpecAuth::ScramLogin {
                username: "selene".into(),
                auth_source: None,
                mechanism: None,
            },
            tls: TlsConfig::default(),
            read_only: false,
        }
    }

    #[tokio::test]
    async fn builds_from_host_and_port() {
        let opts = build_options(&spec(), &Secret::new("pw"))
            .await
            .expect("options build");
        assert_eq!(
            opts.hosts,
            vec![ServerAddress::Tcp {
                host: "db.example.invalid".into(),
                port: Some(27017),
            }]
        );
        // A SCRAM credential was applied with the default `admin` source.
        let cred = opts.credential.expect("credential present");
        assert_eq!(cred.username.as_deref(), Some("selene"));
        assert_eq!(cred.source.as_deref(), Some("admin"));
        assert_eq!(opts.app_name.as_deref(), Some("Selene"));
    }

    #[tokio::test]
    async fn explicit_port_overrides_default() {
        let mut s = spec();
        s.port = Some(27020);
        let opts = build_options(&s, &Secret::new("pw"))
            .await
            .expect("options build");
        assert_eq!(
            opts.hosts,
            vec![ServerAddress::Tcp {
                host: "db.example.invalid".into(),
                port: Some(27020),
            }]
        );
    }

    #[tokio::test]
    async fn builds_from_a_uri() {
        let mut s = spec();
        s.uri = Some("mongodb://user:pw@uri-host.invalid:27099/appdb".into());
        let opts = build_options(&s, &Secret::new("ignored"))
            .await
            .expect("uri options build");
        assert_eq!(
            opts.hosts,
            vec![ServerAddress::Tcp {
                host: "uri-host.invalid".into(),
                port: Some(27099),
            }]
        );
        // The URI's own credential wins over the spec's SCRAM overlay.
        let cred = opts.credential.expect("uri credential present");
        assert_eq!(cred.username.as_deref(), Some("user"));
    }

    #[tokio::test]
    async fn empty_host_and_empty_uri_is_rejected() {
        let mut s = spec();
        s.host = "   ".into();
        s.uri = None;
        let err = build_options(&s, &Secret::new("pw"))
            .await
            .expect_err("empty host + empty uri must be rejected");
        assert!(matches!(err, CoreError::Config(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn auth_none_yields_no_credential() {
        let mut s = spec();
        s.auth = SpecAuth::None;
        let opts = build_options(&s, &Secret::new(""))
            .await
            .expect("anonymous options build");
        assert!(opts.credential.is_none());
    }

    #[tokio::test]
    async fn explicit_mechanism_is_parsed() {
        let mut s = spec();
        s.auth = SpecAuth::ScramLogin {
            username: "selene".into(),
            auth_source: Some("myauth".into()),
            mechanism: Some("SCRAM-SHA-256".into()),
        };
        let opts = build_options(&s, &Secret::new("pw"))
            .await
            .expect("options build");
        let cred = opts.credential.expect("credential present");
        assert_eq!(cred.source.as_deref(), Some("myauth"));
        assert_eq!(cred.mechanism, Some(AuthMechanism::ScramSha256));
    }

    // NOTE: there is deliberately no "password not in Debug" test here. The
    // mongodb driver's `Credential`/`ClientOptions` Debug print the password
    // verbatim (like sqlx's options), so such a test would (correctly) fail. The
    // driver contract is instead that the options value is *never* Debug-
    // formatted or logged — it is only handed to `Client::with_options`. The
    // module docs flag this hazard so future edits do not log the options.
}
