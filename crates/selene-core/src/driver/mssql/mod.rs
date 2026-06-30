//! Microsoft SQL Server driver, backed by [`tiberius`] over Tokio.
//!
//! Layout:
//! - [`config`]   — `ConnectionSpec` + `Secret` → `tiberius::Config`.
//! - [`convert`]  — TDS values/columns → Selene's neutral [`CellValue`]/[`Column`].
//! - [`stream`]   — drives a `QueryStream`, batching rows to a [`RowSink`].
//! - [`introspect`] — catalog queries (databases/schemas/tables/columns).
//! - [`error`]    — tiberius errors → [`CoreError`] (never leaking secrets).
//!
//! Cancellation is cooperative (see [`stream`]); on top of it the Tauri layer
//! drops the connection to raise a server-side Attention for a hard stop.

mod backup;
mod config;
mod convert;
mod error;
mod import;
mod introspect;
mod stream;

use std::time::{Duration, Instant};

use socket2::{SockRef, TcpKeepalive};
use tiberius::{Client, SqlBrowser};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::backup::{
    BackupFile, BackupOptions, DbFile, DefaultDirs, FileMove, RestoreOptions, ServerDirEntry,
};
use crate::capabilities::DriverCapabilities;
use crate::connection_spec::{ConnectionSpec, DriverId};
use crate::driver::{
    CancelToken, Connection, DatabaseDriver, ExecOptions, ExecOutcome, ImportTarget, NewColumn,
    RowSink, RowSource, TestReport,
};
use crate::error::CoreError;
use crate::introspect::{ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo};
use crate::secret::Secret;

use self::config::build_config;
use self::error::{map_connect_err, map_ddl_lock_err, map_tiberius_err};
use self::stream::{run_exec_counting, run_query, TiberiusClient};

/// The MSSQL backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct MssqlDriver;

impl MssqlDriver {
    pub fn new() -> Self {
        Self
    }

    /// Capabilities of the SQL Server backend.
    pub const fn caps() -> DriverCapabilities {
        DriverCapabilities {
            schemas: true,
            multiple_result_sets: true,
            server_side_cancel: true,
            transactions: true,
            explain_plan: true,
            streaming_rows: true,
            list_databases: true,
            data_editing: false,
        }
    }
}

/// Upper bound on how long the whole connect (TCP dial + TLS handshake + login)
/// may take before we give up. Without it a dead/firewalled host can hang for
/// the OS default SYN timeout (often 75s+); a desktop user wants a prompt error.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// TCP keepalive: start probing after 15s idle, then every 5s. On a link that
/// dropped *silently* (no FIN/RST — Wi-Fi off, VPN down, cable pulled) a blocked
/// socket read would otherwise wait for the OS default keepalive idle (~2h).
/// With these settings the kernel tears the socket down within tens of seconds,
/// so any in-flight `execute`/`ping`/introspection call fails instead of hanging
/// forever and pinning the session mutex. This is the backstop; the app-level
/// heartbeat (Tauri layer) is the primary, faster detector for idle sessions.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(15);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Enable TCP keepalive on `tcp`. Best-effort: a failure to set the option is
/// not fatal (the connection still works; we just lose the fast dead-link
/// detection), so we ignore the error rather than refusing to connect.
fn apply_keepalive(tcp: &TcpStream) {
    let keepalive = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    let _ = SockRef::from(tcp).set_tcp_keepalive(&keepalive);
}

/// Open a TDS connection from a spec + secret.
///
/// For a named instance we resolve the real port through the SQL Browser
/// (`SqlBrowser::connect_named`, available via the `sql-browser-tokio` feature
/// and working cross-platform); otherwise we dial the configured host/port
/// directly. tiberius then performs the TLS handshake and login.
///
/// The whole sequence is bounded by [`CONNECT_TIMEOUT`] and the socket gets TCP
/// keepalive ([`apply_keepalive`]) so a later silent link drop is detected
/// promptly rather than hanging a read indefinitely.
async fn open_client(spec: &ConnectionSpec, secret: &Secret) -> Result<TiberiusClient, CoreError> {
    tokio::time::timeout(CONNECT_TIMEOUT, open_client_inner(spec, secret))
        .await
        .map_err(|_| {
            CoreError::Connection(format!(
                "timed out after {}s connecting to the server",
                CONNECT_TIMEOUT.as_secs()
            ))
        })?
}

async fn open_client_inner(
    spec: &ConnectionSpec,
    secret: &Secret,
) -> Result<TiberiusClient, CoreError> {
    let config = build_config(spec, secret)?;

    // Establish the raw TCP stream. `connect_named` performs SQL Browser
    // discovery for named instances (and already sets TCP_NODELAY); the plain
    // path needs nodelay set explicitly to avoid Nagle-induced latency.
    let tcp = if spec.instance.as_deref().is_some_and(|i| !i.is_empty()) {
        TcpStream::connect_named(&config)
            .await
            .map_err(map_connect_err)?
    } else {
        let tcp = TcpStream::connect(config.get_addr())
            .await
            .map_err(|e| CoreError::Connection(format!("{:?}: {e}", e.kind())))?;
        tcp.set_nodelay(true)
            .map_err(|e| CoreError::Connection(format!("set_nodelay: {e}")))?;
        tcp
    };
    apply_keepalive(&tcp);

    // tiberius drives the futures-io traits, so wrap Tokio's stream with the
    // compat shim before handing it over.
    let client = Client::connect(config, tcp.compat_write())
        .await
        .map_err(map_connect_err)?;

    Ok(client)
}

#[async_trait::async_trait]
impl DatabaseDriver for MssqlDriver {
    fn id(&self) -> DriverId {
        DriverId::Mssql
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
        let mut client = open_client(spec, secret).await?;

        // `@@VERSION` is a single-row, single-column scalar — cheap and present
        // on every supported server. `simple_query` (plain batch) is fine for a
        // static, non-parameterised statement.
        let server_version = {
            let row = client
                .simple_query("SELECT @@VERSION")
                .await
                .map_err(map_tiberius_err)?
                .into_row()
                .await
                .map_err(map_tiberius_err)?;
            row.and_then(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
        };

        let elapsed_ms = started.elapsed().as_millis() as u64;

        // Close cleanly; ignore a close error — the test already succeeded and
        // the report should reflect connectivity, not teardown noise.
        let _ = client.close().await;

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
        Ok(Box::new(MssqlConnection { client }))
    }
}

/// A live SQL Server connection. Holds the tiberius client exclusively; the
/// pool guarantees one task drives it at a time (`Connection: Send`, not
/// `Sync`).
struct MssqlConnection {
    client: TiberiusClient,
}

#[async_trait::async_trait]
impl Connection for MssqlConnection {
    async fn execute(
        &mut self,
        sql: &str,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
        cancel: &CancelToken,
    ) -> Result<ExecOutcome, CoreError> {
        // A leading `USE <db>` changes the connection's database context. The
        // affected-count path below runs via tiberius' `execute()`
        // (`sp_executesql`), inside which a `USE` only scopes that dynamic batch
        // and does NOT persist the context to the session. So when the batch is
        // `USE …; <countable DML>`, run the `USE` statement(s) on the persistent
        // plain-batch path first, then count the remaining DML. This makes a
        // batch like `USE web02; BEGIN TRAN; INSERT …; ROLLBACK` report
        // per-statement affected-row counts *and* actually switch the database.
        if let Some((use_statements, remainder)) = crate::guard::peel_leading_use(sql) {
            if !remainder.trim().is_empty() && crate::guard::is_countable_dml_batch(remainder) {
                if cancel.is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                for use_stmt in use_statements {
                    // Plain batch (not `sp_executesql`) so the context persists;
                    // drain the (empty) result stream to complete the round-trip.
                    self.client
                        .simple_query(use_stmt)
                        .await
                        .map_err(map_tiberius_err)?
                        .into_results()
                        .await
                        .map_err(map_tiberius_err)?;
                }
                return run_exec_counting(&mut self.client, remainder, sink, cancel).await;
            }
        }

        // A pure data-modification batch (no rows to return) goes through
        // tiberius' `execute()` so we can report affected-row counts — the
        // streaming `simple_query` path cannot (tiberius drops the DONE count).
        // Everything else (SELECT, EXEC, DDL, USE, transactions, OUTPUT-DML,
        // mixed batches) streams rows as before. See `run_exec_counting`.
        if crate::guard::is_countable_dml_batch(sql) {
            run_exec_counting(&mut self.client, sql, sink, cancel).await
        } else {
            run_query(&mut self.client, sql, opts, sink, cancel).await
        }
    }

    async fn list_databases(&mut self) -> Result<Vec<DatabaseInfo>, CoreError> {
        introspect::list_databases(&mut self.client).await
    }

    async fn list_schemas(&mut self, database: &str) -> Result<Vec<SchemaInfo>, CoreError> {
        introspect::list_schemas(&mut self.client, database).await
    }

    async fn list_tables(
        &mut self,
        database: &str,
        schema: &str,
    ) -> Result<Vec<TableInfo>, CoreError> {
        introspect::list_tables(&mut self.client, database, schema).await
    }

    async fn list_columns(
        &mut self,
        database: &str,
        schema: &str,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, CoreError> {
        introspect::list_columns(&mut self.client, database, schema, table).await
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        // A trivial round-trip that forces a full request/response cycle and so
        // surfaces a dropped socket. We must drain the stream to completion.
        let _ = self
            .client
            .simple_query("SELECT 1")
            .await
            .map_err(map_tiberius_err)?
            .into_row()
            .await
            .map_err(map_tiberius_err)?;
        Ok(())
    }

    async fn current_database(&mut self) -> Result<String, CoreError> {
        let row = self
            .client
            .simple_query("SELECT DB_NAME()")
            .await
            .map_err(map_tiberius_err)?
            .into_row()
            .await
            .map_err(map_tiberius_err)?;
        Ok(row
            .as_ref()
            .and_then(|r| r.get::<&str, _>(0))
            .unwrap_or("")
            .to_string())
    }

    async fn use_database(&mut self, database: &str) -> Result<(), CoreError> {
        let sql = format!("USE {}", introspect::quote_ident(database));
        self.client
            .simple_query(&sql)
            .await
            .map_err(map_tiberius_err)?
            .into_results()
            .await
            .map_err(map_tiberius_err)?;
        Ok(())
    }

    async fn create_database(&mut self, database: &str) -> Result<(), CoreError> {
        let sql = format!("CREATE DATABASE {}", introspect::quote_ident(database));
        self.client
            .simple_query(&sql)
            .await
            .map_err(map_tiberius_err)?
            .into_results()
            .await
            .map_err(map_tiberius_err)?;
        Ok(())
    }

    async fn drop_database(&mut self, database: &str) -> Result<(), CoreError> {
        // Run from `master` so we're not sitting in the database we're dropping.
        // No ROLLBACK IMMEDIATE: if other connections hold it, SQL Server's error
        // surfaces rather than silently terminating their sessions.
        let sql = format!(
            "USE master; DROP DATABASE {}",
            introspect::quote_ident(database),
        );
        self.client
            .simple_query(&sql)
            .await
            .map_err(map_tiberius_err)?
            .into_results()
            .await
            .map_err(map_tiberius_err)?;
        Ok(())
    }

    async fn rename_database(
        &mut self,
        from: &str,
        to: &str,
        force: bool,
    ) -> Result<(), CoreError> {
        let from_q = introspect::quote_ident(from);
        let to_q = introspect::quote_ident(to);
        // Run from `master` so we never alter the database in our own context.
        let sql = if force {
            // Force path: drop the other sessions (ROLLBACK IMMEDIATE), rename,
            // then restore MULTI_USER. The TRY/CATCH guarantees we don't leave
            // the source stuck in SINGLE_USER if the rename itself fails (e.g.
            // the target name already exists): it restores MULTI_USER and
            // re-raises. After a successful MODIFY NAME the db is `to`, so the
            // trailing MULTI_USER targets the new name; on failure it is still
            // `from`. (A racing session could grab the single-user slot between
            // statements, but they run back-to-back on this `master` connection,
            // so the window is negligible for a desktop client.)
            format!(
                "USE master; \
                 ALTER DATABASE {from_q} SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
                 BEGIN TRY \
                 ALTER DATABASE {from_q} MODIFY NAME = {to_q}; \
                 END TRY \
                 BEGIN CATCH \
                 ALTER DATABASE {from_q} SET MULTI_USER; \
                 THROW; \
                 END CATCH; \
                 ALTER DATABASE {to_q} SET MULTI_USER;"
            )
        } else {
            // Clean path: a short lock timeout turns an indefinite wait on an
            // in-use database into a prompt error (1222), which `map_ddl_lock_err`
            // maps to `DatabaseInUse` so the UI can offer the forced retry.
            format!(
                "USE master; SET LOCK_TIMEOUT 5000; ALTER DATABASE {from_q} MODIFY NAME = {to_q};"
            )
        };
        self.client
            .simple_query(&sql)
            .await
            .map_err(map_ddl_lock_err)?
            .into_results()
            .await
            .map_err(map_ddl_lock_err)?;
        Ok(())
    }

    async fn set_database_online(&mut self, database: &str, online: bool) -> Result<(), CoreError> {
        // OFFLINE uses ROLLBACK IMMEDIATE to terminate other connections at once
        // rather than waiting for them to close. Run from `master` so we're not
        // sitting in the database we're taking offline.
        let target = if online {
            "ONLINE"
        } else {
            "OFFLINE WITH ROLLBACK IMMEDIATE"
        };
        let sql = format!(
            "USE master; ALTER DATABASE {} SET {}",
            introspect::quote_ident(database),
            target,
        );
        self.client
            .simple_query(&sql)
            .await
            .map_err(map_tiberius_err)?
            .into_results()
            .await
            .map_err(map_tiberius_err)?;
        Ok(())
    }

    async fn create_table(
        &mut self,
        database: Option<&str>,
        schema: &str,
        table: &str,
        columns: &[NewColumn],
        cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        import::create_table(&mut self.client, database, schema, table, columns, cancel).await
    }

    async fn drop_table(
        &mut self,
        database: Option<&str>,
        schema: &str,
        table: &str,
        cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        import::drop_table(&mut self.client, database, schema, table, cancel).await
    }

    async fn import_rows(
        &mut self,
        target: &ImportTarget,
        source: &mut dyn RowSource,
        atomic: bool,
        batch_size: usize,
        cancel: &CancelToken,
    ) -> Result<u64, CoreError> {
        import::import_rows(&mut self.client, target, source, atomic, batch_size, cancel).await
    }

    async fn backup_database(
        &mut self,
        database: &str,
        to_path: &str,
        opts: &BackupOptions,
        _cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        backup::backup_database(&mut self.client, database, to_path, opts).await
    }

    async fn restore_filelist(&mut self, from_path: &str) -> Result<Vec<BackupFile>, CoreError> {
        backup::restore_filelist(&mut self.client, from_path).await
    }

    async fn database_files(&mut self, database: &str) -> Result<Vec<DbFile>, CoreError> {
        backup::database_files(&mut self.client, database).await
    }

    async fn default_file_dirs(&mut self) -> Result<DefaultDirs, CoreError> {
        backup::default_file_dirs(&mut self.client).await
    }

    async fn default_backup_dir(&mut self) -> Result<String, CoreError> {
        backup::default_backup_dir(&mut self.client).await
    }

    async fn list_server_dir(&mut self, path: &str) -> Result<Vec<ServerDirEntry>, CoreError> {
        backup::list_server_dir(&mut self.client, path).await
    }

    async fn restore_database(
        &mut self,
        target: &str,
        from_path: &str,
        moves: &[FileMove],
        opts: &RestoreOptions,
        _cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        backup::restore_database(&mut self.client, target, from_path, moves, opts).await
    }

    async fn current_session_id(&mut self) -> Result<i32, CoreError> {
        backup::current_session_id(&mut self.client).await
    }

    async fn backup_percent_complete(&mut self, spid: i32) -> Result<Option<f32>, CoreError> {
        backup::request_percent_complete(&mut self.client, spid).await
    }

    async fn kill_session(&mut self, spid: i32) -> Result<(), CoreError> {
        backup::kill_session(&mut self.client, spid).await
    }
}
