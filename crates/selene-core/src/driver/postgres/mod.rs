//! PostgreSQL driver, backed by [`sqlx`].
//!
//! Layout (mirrors the sqlite + mssql drivers):
//! - [`config`]     — `ConnectionSpec` + `Secret` → [`PgConnectOptions`](sqlx::postgres::PgConnectOptions).
//! - [`convert`]    — sqlx values/columns → Selene's neutral [`CellValue`](crate::value::CellValue)/[`Column`](crate::value::Column).
//! - [`stream`]     — drives `fetch_many` through the shared streaming pump.
//! - [`introspect`] — catalog reads (`current_database()`, `pg_namespace`, `information_schema`).
//! - [`import`]     — `CREATE TABLE` / `DROP TABLE` / bound multi-row `INSERT` (positional `$N`).
//! - [`error`]      — `sqlx::Error` → [`CoreError`].
//!
//! A connection holds a single [`PgConnection`] (not a pool): Selene's session
//! layer owns connection lifetime, and `Connection: Send` (not `Sync`) matches
//! sqlx's single-connection handle. Postgres has a real schema level and
//! transactions but **cannot switch databases on a live connection** (no `USE`),
//! which the driver's [`capabilities`](DriverCapabilities) and `use_database`
//! handling reflect.

mod config;
mod convert;
mod error;
mod import;
mod introspect;
mod stream;

use std::time::Instant;

use sqlx::postgres::PgConnection;
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
use self::error::{map_connect_err, map_sqlx_err};

/// The PostgreSQL backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct PostgresDriver;

impl PostgresDriver {
    pub fn new() -> Self {
        Self
    }

    /// Capabilities of the PostgreSQL backend. Postgres has a schema level,
    /// transactions, and `EXPLAIN`, streams rows incrementally, but exposes only
    /// the connected database (no live `USE`), runs one statement's result set at
    /// a time over the simple-query stream, and Selene does not yet wire its
    /// server-side cancel / data-editing / backup / database-admin features.
    pub const fn caps() -> DriverCapabilities {
        DriverCapabilities {
            schemas: true,
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

/// Open a [`PgConnection`] from a spec + secret.
async fn open_connection(
    spec: &ConnectionSpec,
    secret: &Secret,
) -> Result<PgConnection, CoreError> {
    let options = build_options(spec, secret)?;
    options.connect().await.map_err(map_connect_err)
}

#[async_trait::async_trait]
impl DatabaseDriver for PostgresDriver {
    fn id(&self) -> DriverId {
        DriverId::Postgres
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
        let mut conn = open_connection(spec, secret).await?;

        // `version()` is a single-row scalar present on every server (e.g.
        // "PostgreSQL 16.2 on x86_64-pc-linux-gnu …").
        let server_version = {
            let row = sqlx::query("SELECT version() AS v")
                .fetch_one(&mut conn)
                .await
                .map_err(map_sqlx_err)?;
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
        secret: &Secret,
    ) -> Result<Box<dyn Connection>, CoreError> {
        let conn = open_connection(spec, secret).await?;
        Ok(Box::new(PostgresConnectionHandle { conn }))
    }
}

/// A live Postgres connection. Holds the sqlx handle exclusively; the session
/// layer guarantees one task drives it at a time (`Connection: Send`, not
/// `Sync`).
struct PostgresConnectionHandle {
    conn: PgConnection,
}

#[async_trait::async_trait]
impl Connection for PostgresConnectionHandle {
    async fn execute(
        &mut self,
        sql: &str,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
        cancel: &CancelToken,
    ) -> Result<ExecOutcome, CoreError> {
        // Postgres needs none of the mssql USE/DML-count special-casing: the
        // shared pump derives result-set boundaries and DML affected-row counts
        // straight from the `fetch_many` stream.
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
            .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn current_database(&mut self) -> Result<String, CoreError> {
        let row = sqlx::query("SELECT current_database() AS name")
            .fetch_one(&mut self.conn)
            .await
            .map_err(map_sqlx_err)?;
        row.try_get::<String, _>("name").map_err(map_sqlx_err)
    }

    // `use_database` keeps the trait default (Unsupported): a Postgres connection
    // is bound to one database for its lifetime — there is no `USE`. Switching
    // databases requires opening a new connection to the target database, which
    // the session layer drives, not this method.

    async fn create_table(
        &mut self,
        _database: Option<&str>,
        schema: &str,
        table: &str,
        columns: &[NewColumn],
        cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        import::create_table(&mut self.conn, schema, table, columns, cancel).await
    }

    async fn drop_table(
        &mut self,
        _database: Option<&str>,
        schema: &str,
        table: &str,
        cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        import::drop_table(&mut self.conn, schema, table, cancel).await
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
    // trait defaults (Unsupported) — Postgres supports none of them here.
}
