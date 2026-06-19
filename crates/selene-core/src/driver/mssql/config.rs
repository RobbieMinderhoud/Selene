//! Builds a [`tiberius::Config`] from Selene's [`ConnectionSpec`] + [`Secret`].
//!
//! This is the single place where the secret is exposed: it is read with
//! [`Secret::expose`] only to hand it to `tiberius::AuthMethod::sql_server`, and
//! is never stored, logged, or formatted. The resulting `Config`'s `Debug`
//! impl redacts the password (tiberius prints `<HIDDEN>`), so it is safe to
//! trace.

use tiberius::{AuthMethod, Config, EncryptionLevel};

use crate::connection_spec::{AuthMethod as SpecAuth, ConnectionSpec};
use crate::error::CoreError;
use crate::secret::Secret;

/// Translate a [`ConnectionSpec`] and its [`Secret`] into a `tiberius::Config`.
///
/// Encryption is enabled by default (`spec.tls.encrypt`); certificate
/// validation is only skipped when `spec.tls.trust_server_certificate` is set —
/// an explicit, UI-warned opt-in for self-signed dev servers.
pub fn build_config(spec: &ConnectionSpec, secret: &Secret) -> Result<Config, CoreError> {
    let mut config = Config::new();

    if spec.host.trim().is_empty() {
        return Err(CoreError::Config("host must not be empty".into()));
    }
    config.host(&spec.host);

    // A user-set port always wins. When a named instance is used without an
    // explicit port, we leave the port unset so the SQL Browser (resolved in
    // `connect`) can supply it; tiberius defaults the browser probe to 1434.
    if let Some(port) = spec.port {
        config.port(port);
    } else if spec.instance.is_none() {
        // No instance and no explicit port: dial the conventional MSSQL port.
        if let Some(port) = spec.effective_port() {
            config.port(port);
        }
    }

    if let Some(instance) = spec.instance.as_deref() {
        if !instance.is_empty() {
            config.instance_name(instance);
        }
    }

    if let Some(database) = spec.database.as_deref() {
        if !database.is_empty() {
            config.database(database);
        }
    }

    // Authentication. Only SQL logins are modelled today. `AuthMethod` is
    // `#[non_exhaustive]` for downstream crates, but within `selene-core` the
    // match is exhaustive; when a new variant (Integrated, Entra ID, …) is
    // added here, the compiler will force us to handle it.
    match &spec.auth {
        SpecAuth::SqlLogin { username } => {
            if username.trim().is_empty() {
                return Err(CoreError::Config("username must not be empty".into()));
            }
            // The ONLY point the secret is exposed — handed straight to tiberius.
            config.authentication(AuthMethod::sql_server(username, secret.expose()));
        }
    }

    // Transport security. `encrypt` toggles between full encryption and
    // login-only encryption; we never disable encryption entirely
    // (`NotSupported`), as that would send query traffic in cleartext.
    if spec.tls.encrypt {
        config.encryption(EncryptionLevel::Required);
    } else {
        config.encryption(EncryptionLevel::Off);
    }

    // Only relax certificate validation on explicit opt-in. `trust_cert` panics
    // if `trust_cert_ca` was set before, but we never call the latter, so this
    // is safe.
    if spec.tls.trust_server_certificate {
        config.trust_cert();
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_spec::{DriverId, TlsConfig};

    fn spec() -> ConnectionSpec {
        ConnectionSpec {
            id: "c1".into(),
            name: "Test".into(),
            driver: DriverId::Mssql,
            host: "db.example.invalid".into(),
            port: None,
            instance: None,
            database: Some("appdb".into()),
            auth: SpecAuth::SqlLogin {
                username: "sa".into(),
            },
            tls: TlsConfig::default(),
            read_only: false,
        }
    }

    #[test]
    fn builds_with_defaults() {
        let s = spec();
        let cfg = build_config(&s, &Secret::new("pw")).expect("config builds");
        // Default port for MSSQL with no instance.
        assert_eq!(cfg.get_addr(), "db.example.invalid:1433");
        // Debug must never leak the password.
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("pw"),
            "config Debug leaked the password: {dbg}"
        );
    }

    #[test]
    fn empty_host_is_rejected() {
        let mut s = spec();
        s.host = "   ".into();
        let err = build_config(&s, &Secret::new("pw")).unwrap_err();
        assert!(matches!(err, CoreError::Config(_)));
    }

    #[test]
    fn empty_username_is_rejected() {
        let mut s = spec();
        s.auth = SpecAuth::SqlLogin {
            username: "".into(),
        };
        let err = build_config(&s, &Secret::new("pw")).unwrap_err();
        assert!(matches!(err, CoreError::Config(_)));
    }

    #[test]
    fn explicit_port_overrides_default() {
        let mut s = spec();
        s.port = Some(14333);
        let cfg = build_config(&s, &Secret::new("pw")).expect("config builds");
        assert_eq!(cfg.get_addr(), "db.example.invalid:14333");
    }

    #[test]
    fn named_instance_without_port_uses_browser_port() {
        let mut s = spec();
        s.instance = Some("SQLEXPRESS".into());
        s.port = None;
        let cfg = build_config(&s, &Secret::new("pw")).expect("config builds");
        // With an instance and no explicit port, tiberius probes SQL Browser on
        // 1434 (the port is later replaced by the browser-resolved value).
        assert_eq!(cfg.get_addr(), "db.example.invalid:1434");
    }
}
