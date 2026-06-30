//! The (non-secret) description of a database connection.
//!
//! A `ConnectionSpec` holds everything needed to connect *except* the password,
//! which travels separately as a [`Secret`](crate::Secret) and lives only in
//! the OS keychain. That separation is what makes it safe to persist and
//! `Debug`-print a `ConnectionSpec`.

use serde::{Deserialize, Serialize};

/// Which database backend a connection targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverId {
    Mssql,
    /// Reserved for the sqlx-based drivers added in v0.3.
    Postgres,
    Mysql,
    Sqlite,
}

impl DriverId {
    /// The conventional default TCP port, if the backend uses one.
    pub fn default_port(self) -> Option<u16> {
        match self {
            DriverId::Mssql => Some(1433),
            DriverId::Postgres => Some(5432),
            DriverId::Mysql => Some(3306),
            DriverId::Sqlite => None,
        }
    }
}

/// How to authenticate. Marked `#[non_exhaustive]` so Windows Integrated Auth
/// and Azure AD / Entra ID can be added later without a breaking change.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthMethod {
    /// SQL Server login: username here, password supplied separately as a
    /// [`Secret`](crate::Secret).
    SqlLogin { username: String },
    /// No authentication (e.g. a local SQLite file, whose "host" is the file
    /// path). The connection carries no username, port, or password.
    None,
}

/// Transport security settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Encrypt the connection. On by default.
    #[serde(default = "default_true")]
    pub encrypt: bool,
    /// Skip server-certificate validation (e.g. self-signed dev servers).
    /// Off by default; enabling it is an explicit, warned opt-in in the UI.
    #[serde(default)]
    pub trust_server_certificate: bool,
}

fn default_true() -> bool {
    true
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            encrypt: true,
            trust_server_certificate: false,
        }
    }
}

/// A saved connection's non-secret configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectionSpec {
    /// Stable identifier (also the keychain account key for the secret).
    pub id: String,
    /// Human-friendly display name.
    pub name: String,
    /// Target backend.
    pub driver: DriverId,
    /// Server host or address.
    pub host: String,
    /// Port; falls back to the driver default when `None`.
    #[serde(default)]
    pub port: Option<u16>,
    /// Named instance (MSSQL), if any.
    #[serde(default)]
    pub instance: Option<String>,
    /// Default database to connect to, if any.
    #[serde(default)]
    pub database: Option<String>,
    /// Authentication method.
    pub auth: AuthMethod,
    /// Transport security.
    #[serde(default)]
    pub tls: TlsConfig,
    /// When true, the SQL guard blocks any non-SELECT statement for this
    /// connection (a defence-in-depth toggle for production servers).
    #[serde(default)]
    pub read_only: bool,
}

impl ConnectionSpec {
    /// The port to dial, applying the driver default when unset.
    pub fn effective_port(&self) -> Option<u16> {
        self.port.or_else(|| self.driver.default_port())
    }

    /// The login username, if the auth method has one.
    pub fn username(&self) -> Option<&str> {
        match &self.auth {
            AuthMethod::SqlLogin { username } => Some(username),
            AuthMethod::None => None,
        }
    }
}
