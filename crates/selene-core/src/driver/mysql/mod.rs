//! MySQL / MariaDB driver, backed by [`sqlx`].
//!
//! Layout (mirrors the sqlite + postgres + mssql drivers):
//! - [`config`]     — `ConnectionSpec` + `Secret` → [`MySqlConnectOptions`](sqlx::mysql::MySqlConnectOptions).
//! - [`convert`]    — sqlx values/columns → Selene's neutral [`CellValue`](crate::value::CellValue)/[`Column`](crate::value::Column).
//! - [`stream`]     — drives `fetch_many` through the shared streaming pump.
//! - [`introspect`] — catalog reads (`information_schema`, `DATABASE()`).
//! - [`import`]     — `CREATE TABLE` / `DROP TABLE` / bound multi-row `INSERT` (`?` placeholders).
//! - [`error`]      — `sqlx::Error` → [`CoreError`].
//!
//! A connection holds a single [`MySqlConnection`] (not a pool): Selene's session
//! layer owns connection lifetime, and `Connection: Send` (not `Sync`) matches
//! sqlx's single-connection handle.
//!
//! ## MySQL specifics vs Postgres
//! - **database == schema**: MySQL has no schema namespace between database and
//!   table, so `capabilities.schemas` is `false` and the introspection collapses
//!   the schema level (it returns an empty schema list).
//! - **`USE` works**: unlike Postgres, a MySQL connection *can* switch its active
//!   database on the fly, so `use_database` is implemented (not Unsupported).
//!
//! ⚠️ Like `PgConnectOptions`, sqlx's `MySqlConnectOptions` `Debug` is **not**
//! redaction-safe (it prints the password). The options are therefore only ever
//! `.connect()`-ed, never logged or `Debug`-formatted — see [`config`].

mod config;
mod convert;
mod error;
mod import;
mod introspect;
mod stream;

use std::time::Instant;

use sqlx::mysql::MySqlConnection;
use sqlx::{ConnectOptions as _, Connection as _, Executor as _, Row as _};

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
use self::introspect::quote_ident;

/// The MySQL / MariaDB backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct MysqlDriver;

impl MysqlDriver {
    pub fn new() -> Self {
        Self
    }

    /// Capabilities of the MySQL backend. MySQL has **no schema level**
    /// (database == schema), supports transactions and `EXPLAIN`, streams rows
    /// incrementally, and lists all databases on the server. It runs one
    /// statement's result set at a time over the stream, and Selene does not yet
    /// wire its server-side cancel / data-editing / backup / database-admin
    /// features.
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

/// Open a [`MySqlConnection`] from a spec + secret.
async fn open_connection(
    spec: &ConnectionSpec,
    secret: &Secret,
) -> Result<MySqlConnection, CoreError> {
    let options = build_options(spec, secret)?;
    options.connect().await.map_err(map_connect_err)
}

#[async_trait::async_trait]
impl DatabaseDriver for MysqlDriver {
    fn id(&self) -> DriverId {
        DriverId::Mysql
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

        // `VERSION()` is a single-row scalar present on every server (e.g.
        // "8.1.0" / "10.11.2-MariaDB").
        let server_version = {
            let row = sqlx::query("SELECT VERSION() AS v")
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
        Ok(Box::new(MysqlConnectionHandle { conn }))
    }
}

/// A live MySQL connection. Holds the sqlx handle exclusively; the session layer
/// guarantees one task drives it at a time (`Connection: Send`, not `Sync`).
struct MysqlConnectionHandle {
    conn: MySqlConnection,
}

#[async_trait::async_trait]
impl Connection for MysqlConnectionHandle {
    async fn execute(
        &mut self,
        sql: &str,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
        cancel: &CancelToken,
    ) -> Result<ExecOutcome, CoreError> {
        // MySQL needs none of the mssql USE/DML-count special-casing: the shared
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
            .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn current_database(&mut self) -> Result<String, CoreError> {
        // Run via `raw_sql` (the text protocol), NOT a prepared `sqlx::query`: a
        // cached prepared `SELECT DATABASE()` returns the database that was active
        // when it was first prepared, so it would not reflect a later
        // `use_database`. The text protocol evaluates `DATABASE()` fresh each call.
        //
        // `DATABASE()` returns NULL when no database is selected; map that to an
        // empty string (the trait's convention for "no active database"). Read by
        // ordinal since the expression has no stable column name.
        let row = self
            .conn
            .fetch_one(sqlx::raw_sql("SELECT DATABASE()"))
            .await
            .map_err(map_sqlx_err)?;
        let name: Option<String> = row.try_get(0).map_err(map_sqlx_err)?;
        Ok(name.unwrap_or_default())
    }

    async fn use_database(&mut self, database: &str) -> Result<(), CoreError> {
        // Unlike Postgres, MySQL can switch the active database on a live
        // connection. The identifier is backtick-quoted (it comes from the schema
        // tree / user selection) so it cannot break out of the `USE` statement.
        let sql = format!("USE {}", quote_ident(database));
        self.conn
            .execute(sqlx::raw_sql(&sql))
            .await
            .map_err(map_sqlx_err)?;
        Ok(())
    }

    async fn create_table(
        &mut self,
        database: Option<&str>,
        _schema: &str,
        table: &str,
        columns: &[NewColumn],
        cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        import::create_table(&mut self.conn, database, table, columns, cancel).await
    }

    async fn drop_table(
        &mut self,
        database: Option<&str>,
        _schema: &str,
        table: &str,
        cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        import::drop_table(&mut self.conn, database, table, cancel).await
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
    // trait defaults (Unsupported) — MySQL supports none of them here.
}
