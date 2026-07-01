//! MongoDB driver, backed by the official [`mongodb`] crate over Tokio.
//!
//! MongoDB is a document store, not a SQL engine, so it does not fit Selene's
//! relational model cleanly. This driver threads it through the existing
//! [`Connection`]/[`DatabaseDriver`] traits anyway; support arrives in stages:
//!
//! - **M1 (this module)**: connect / ping / `test_connection` / capabilities +
//!   `list_databases`. Query execution and introspection-by-sampling are stubbed
//!   ([`CoreError::Unsupported`] / empty lists) and land in later PRs.
//!
//! Layout:
//! - [`config`] — `ConnectionSpec` + `Secret` → [`mongodb::options::ClientOptions`].
//! - [`error`]  — `mongodb::error::Error` → [`CoreError`] (never leaking secrets).
//!
//! TLS uses the **rustls** backend (never native-tls — it breaks the handshake
//! on macOS, exactly as for the mssql/sqlx drivers).

mod config;
mod error;

use std::time::Instant;

use mongodb::bson::doc;
use mongodb::Client;

use crate::capabilities::DriverCapabilities;
use crate::connection_spec::{ConnectionSpec, DriverId};
use crate::driver::{
    CancelToken, Connection, DatabaseDriver, ExecOptions, ExecOutcome, RowSink, TestReport,
};
use crate::error::CoreError;
use crate::introspect::{ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo};
use crate::secret::Secret;

use self::config::build_options;
use self::error::{map_connect_err, map_mongo_err};

/// Databases MongoDB treats as internal/system.
const SYSTEM_DATABASES: &[&str] = &["admin", "local", "config"];

/// The MongoDB backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct MongodbDriver;

impl MongodbDriver {
    pub fn new() -> Self {
        Self
    }

    /// Capabilities of the MongoDB backend. It has no SQL schema level and no
    /// server-side cancel of a running find, but it does have multi-database
    /// servers, transactions, and (eventually) streamed cursors.
    pub const fn caps() -> DriverCapabilities {
        DriverCapabilities {
            schemas: false,
            multiple_result_sets: false,
            server_side_cancel: false,
            transactions: true,
            explain_plan: false,
            streaming_rows: true,
            list_databases: true,
            data_editing: false,
            backup_restore: false,
            database_create_drop: false,
            database_rename: false,
            database_online_offline: false,
        }
    }
}

/// Build a [`Client`] from a spec + secret and verify liveness with a `{ping:1}`
/// on the `admin` database. The `mongodb::Client` is lazily-connected and
/// internally pooled, so the ping is what actually surfaces a connect failure.
async fn open_client(spec: &ConnectionSpec, secret: &Secret) -> Result<Client, CoreError> {
    let options = build_options(spec, secret).await?;
    // NB: `options` holds the exposed password — never log or Debug it.
    let client = Client::with_options(options).map_err(map_connect_err)?;
    client
        .database("admin")
        .run_command(doc! { "ping": 1 })
        .await
        .map_err(map_connect_err)?;
    Ok(client)
}

#[async_trait::async_trait]
impl DatabaseDriver for MongodbDriver {
    fn id(&self) -> DriverId {
        DriverId::Mongodb
    }

    fn capabilities(&self) -> DriverCapabilities {
        Self::caps()
    }

    async fn test_connection(
        &self,
        spec: &ConnectionSpec,
        secret: &Secret,
    ) -> Result<TestReport, CoreError> {
        let started = Instant::now();
        let options = build_options(spec, secret).await?;
        let client = Client::with_options(options).map_err(map_connect_err)?;

        // `buildInfo` returns the server version banner and doubles as a liveness
        // check (it forces server selection + a round-trip).
        let server_version = {
            let info = client
                .database("admin")
                .run_command(doc! { "buildInfo": 1 })
                .await
                .map_err(map_connect_err)?;
            info.get_str("version").ok().map(str::to_string)
        };

        let elapsed_ms = started.elapsed().as_millis() as u64;

        // The client has no explicit close; dropping it tears down the pool.
        Ok(TestReport {
            server_version,
            elapsed_ms,
        })
    }

    async fn connect(
        &self,
        spec: &ConnectionSpec,
        secret: &Secret,
    ) -> Result<Box<dyn Connection>, CoreError> {
        let client = open_client(spec, secret).await?;
        // Track the spec's default database so later (M2) queries can default
        // their collection's database context.
        let default_db = spec
            .database
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Ok(Box::new(MongodbConnection { client, default_db }))
    }
}

/// A live MongoDB connection. Owns one [`Client`] (which is `Clone` and
/// internally connection-pooled). `default_db` tracks the "current database" the
/// way [`Connection::use_database`] expects — MongoDB has no session-bound
/// current-database concept, so we track it ourselves.
struct MongodbConnection {
    client: Client,
    default_db: Option<String>,
}

#[async_trait::async_trait]
impl Connection for MongodbConnection {
    async fn execute(
        &mut self,
        _sql: &str,
        _opts: &ExecOptions,
        _sink: &mut dyn RowSink,
        _cancel: &CancelToken,
    ) -> Result<ExecOutcome, CoreError> {
        // Query execution (find/aggregate → rows) lands in M2.
        Err(CoreError::Unsupported(
            "MongoDB query execution is not implemented yet".into(),
        ))
    }

    async fn list_databases(&mut self) -> Result<Vec<DatabaseInfo>, CoreError> {
        let names = self
            .client
            .list_database_names()
            .await
            .map_err(map_mongo_err)?;
        Ok(names
            .into_iter()
            .map(|name| {
                let is_system = SYSTEM_DATABASES.contains(&name.as_str());
                DatabaseInfo {
                    name,
                    is_system,
                    // MongoDB has no offline/online database state.
                    state_desc: "ONLINE".to_string(),
                }
            })
            .collect())
    }

    async fn list_schemas(&mut self, _database: &str) -> Result<Vec<SchemaInfo>, CoreError> {
        // MongoDB has no schema level (capabilities advertise `schemas: false`).
        Ok(Vec::new())
    }

    async fn list_tables(
        &mut self,
        _database: &str,
        _schema: &str,
    ) -> Result<Vec<TableInfo>, CoreError> {
        // TODO(mongodb M3): map `listCollections` → collections as tables.
        Ok(Vec::new())
    }

    async fn list_columns(
        &mut self,
        _database: &str,
        _schema: &str,
        _table: &str,
    ) -> Result<Vec<ColumnInfo>, CoreError> {
        // TODO(mongodb M3): infer a column shape by sampling documents.
        Ok(Vec::new())
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        self.client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .await
            .map_err(map_mongo_err)?;
        Ok(())
    }

    async fn current_database(&mut self) -> Result<String, CoreError> {
        Ok(self.default_db.clone().unwrap_or_default())
    }

    async fn use_database(&mut self, database: &str) -> Result<(), CoreError> {
        // MongoDB has no session-bound current database; tracking it lets later
        // queries default their collection's database.
        self.default_db = Some(database.to_string());
        Ok(())
    }

    // All remaining optional methods (create_table/import_rows/backup/restore +
    // database admin) use the trait defaults (Unsupported).
}
