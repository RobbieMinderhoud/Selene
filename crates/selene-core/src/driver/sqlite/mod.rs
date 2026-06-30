//! SQLite driver, backed by [`sqlx`] with the statically-bundled engine.
//!
//! Layout (mirrors the mssql driver):
//! - [`config`]    — `ConnectionSpec` → [`SqliteConnectOptions`].
//! - [`convert`]   — sqlx values/columns → Selene's neutral [`CellValue`]/[`Column`].
//! - [`stream`]    — drives `fetch_many` through the shared streaming pump.
//! - [`introspect`] — catalog reads (`PRAGMA database_list`/`table_info`, `sqlite_master`).
//! - [`import`]    — `CREATE TABLE` / `DROP TABLE` / bound multi-row `INSERT`.
//! - [`error`]     — `sqlx::Error` → [`CoreError`].
//!
//! A connection holds a single [`SqliteConnection`] (not a pool): Selene's
//! session layer owns connection lifetime, and `Connection: Send` (not `Sync`)
//! matches sqlx's single-connection handle. SQLite has no schema level, no
//! server-side cancel, and no multi-database server, which the driver's
//! [`capabilities`](DriverCapabilities) advertise so the UI gates accordingly.

mod config;
mod convert;
mod error;
mod import;
mod introspect;
mod stream;

use std::time::Instant;

use sqlx::sqlite::SqliteConnection;
use sqlx::{ConnectOptions as _, Connection as _, Row as _};

use crate::capabilities::DriverCapabilities;
use crate::connection_spec::{ConnectionSpec, DriverId};
use crate::driver::{
    CancelToken, Connection, DatabaseDriver, ExecOptions, ExecOutcome, ImportTarget, NewColumn,
    RowSink, RowSource, TestReport,
};
use crate::error::CoreError;
use crate::introspect::{ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo};
use crate::secret::Secret;

use self::config::build_options;
use self::error::map_connect_err;

/// The SQLite backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteDriver;

impl SqliteDriver {
    pub fn new() -> Self {
        Self
    }

    /// Capabilities of the SQLite backend. SQLite is a single-file engine with
    /// no schema namespace, no server-side cancellation, and no multi-database
    /// server, but it does have transactions and `EXPLAIN`.
    pub const fn caps() -> DriverCapabilities {
        DriverCapabilities {
            schemas: false,
            multiple_result_sets: false,
            server_side_cancel: false,
            transactions: true,
            explain_plan: true,
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

/// Open a [`SqliteConnection`] from a spec. SQLite needs no secret (the secret
/// argument is accepted to satisfy the trait and is ignored).
async fn open_connection(spec: &ConnectionSpec) -> Result<SqliteConnection, CoreError> {
    let options = build_options(spec)?;
    options.connect().await.map_err(map_connect_err)
}

#[async_trait::async_trait]
impl DatabaseDriver for SqliteDriver {
    fn id(&self) -> DriverId {
        DriverId::Sqlite
    }

    fn capabilities(&self) -> DriverCapabilities {
        Self::caps()
    }

    async fn test_connection(
        &self,
        spec: &ConnectionSpec,
        _secret: &Secret,
    ) -> Result<TestReport, CoreError> {
        let started = Instant::now();
        let mut conn = open_connection(spec).await?;

        // `sqlite_version()` is a single-row scalar present on every build.
        let server_version = {
            let row = sqlx::query("SELECT sqlite_version() AS v")
                .fetch_one(&mut conn)
                .await
                .map_err(error::map_sqlx_err)?;
            row.try_get::<String, _>("v").ok()
        };

        let elapsed_ms = started.elapsed().as_millis() as u64;

        // Close cleanly; ignore a teardown error — the test already succeeded.
        let _ = conn.close().await;

        Ok(TestReport {
            server_version,
            elapsed_ms,
        })
    }

    async fn connect(
        &self,
        spec: &ConnectionSpec,
        _secret: &Secret,
    ) -> Result<Box<dyn Connection>, CoreError> {
        let conn = open_connection(spec).await?;
        Ok(Box::new(SqliteConnectionHandle { conn }))
    }
}

/// A live SQLite connection. Holds the sqlx handle exclusively; the session
/// layer guarantees one task drives it at a time (`Connection: Send`, not
/// `Sync`).
struct SqliteConnectionHandle {
    conn: SqliteConnection,
}

#[async_trait::async_trait]
impl Connection for SqliteConnectionHandle {
    async fn execute(
        &mut self,
        sql: &str,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
        cancel: &CancelToken,
    ) -> Result<ExecOutcome, CoreError> {
        // SQLite needs none of the mssql USE/DML-count special-casing: the shared
        // pump derives result-set boundaries and DML affected-row counts straight
        // from the `fetch_many` stream.
        stream::run_query(&mut self.conn, sql, opts, sink, cancel).await
    }

    async fn list_databases(&mut self) -> Result<Vec<DatabaseInfo>, CoreError> {
        introspect::list_databases(&mut self.conn).await
    }

    async fn list_schemas(&mut self, database: &str) -> Result<Vec<SchemaInfo>, CoreError> {
        introspect::list_schemas(&mut self.conn, database).await
    }

    async fn list_tables(
        &mut self,
        database: &str,
        schema: &str,
    ) -> Result<Vec<TableInfo>, CoreError> {
        introspect::list_tables(&mut self.conn, database, schema).await
    }

    async fn list_columns(
        &mut self,
        database: &str,
        schema: &str,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, CoreError> {
        introspect::list_columns(&mut self.conn, database, schema, table).await
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        // A trivial round-trip that forces a full request/response cycle.
        sqlx::query("SELECT 1")
            .fetch_one(&mut self.conn)
            .await
            .map_err(error::map_sqlx_err)?;
        Ok(())
    }

    async fn current_database(&mut self) -> Result<String, CoreError> {
        // SQLite's primary database is always `main`.
        Ok("main".to_string())
    }

    // `use_database` keeps the trait default (Unsupported): SQLite has no
    // switchable database context (ATTACH is out of scope here).

    async fn create_table(
        &mut self,
        _database: Option<&str>,
        _schema: &str,
        table: &str,
        columns: &[NewColumn],
        cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        import::create_table(&mut self.conn, table, columns, cancel).await
    }

    async fn drop_table(
        &mut self,
        _database: Option<&str>,
        _schema: &str,
        table: &str,
        cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        import::drop_table(&mut self.conn, table, cancel).await
    }

    async fn import_rows(
        &mut self,
        target: &ImportTarget,
        source: &mut dyn RowSource,
        atomic: bool,
        batch_size: usize,
        cancel: &CancelToken,
    ) -> Result<u64, CoreError> {
        import::import_rows(&mut self.conn, target, source, atomic, batch_size, cancel).await
    }

    // All remaining optional methods (backup/restore + database admin) use the
    // trait defaults (Unsupported) — SQLite supports none of them.
}
