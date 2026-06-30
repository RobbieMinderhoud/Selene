//! The driver abstraction — the contract every backend implements and the rest
//! of Selene depends on.
//!
//! Design notes:
//! - Row *production* (the driver) is decoupled from row *transport* (the Tauri
//!   `Channel`) via [`RowSink`], so the core stays UI-agnostic and unit-testable
//!   with an in-memory sink.
//! - Dispatch is dynamic (`Box<dyn Connection>`): the connection registry holds
//!   heterogeneous live connections uniformly. Cargo *features* gate which
//!   drivers are *compiled*; `dyn` decides which is *selected* at runtime.
//! - Cancellation is cooperative here ([`CancelToken`]); the Tauri layer adds a
//!   hard abort on top (tiberius cancels by dropping the connection).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::backup::{
    BackupFile, BackupOptions, DbFile, DefaultDirs, FileMove, RestoreOptions, ServerDirEntry,
};
use crate::capabilities::DriverCapabilities;
use crate::connection_spec::{ConnectionSpec, DriverId};
use crate::error::CoreError;
use crate::introspect::{ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo};
use crate::secret::Secret;
use crate::value::{CellValue, Column};

#[cfg(feature = "mssql")]
pub mod mssql;

// Shared sqlx helpers (streaming pump, param sub-batching, value formatting)
// reused by every sqlx-backed driver. Compiled whenever at least one of them is.
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
mod shared;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "sqlite")]
pub mod sqlite;

/// Whether a [`RowSink`] wants more data or has seen enough (cancel / row cap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Keep streaming.
    Continue,
    /// Stop producing rows; the driver should end the stream promptly.
    Stop,
}

/// A cheap, cloneable cooperative-cancellation flag.
///
/// The driver checks [`CancelToken::is_cancelled`] between row batches and stops
/// when set. This complements — and is backed by — the hard task-abort the
/// Tauri layer performs (dropping a tiberius connection raises a server-side
/// Attention).
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Create a fresh, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Options controlling a single query execution.
#[derive(Clone, Debug)]
pub struct ExecOptions {
    /// Hard cap on rows returned across all result sets; protects against an
    /// accidental unbounded `SELECT *`. `None` means unlimited.
    pub max_rows: Option<u64>,
    /// How many rows to buffer before flushing a batch to the sink.
    pub batch_size: usize,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            max_rows: Some(50_000),
            batch_size: 500,
        }
    }
}

/// Summary of a completed execution.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutcome {
    /// Number of result sets produced.
    pub result_sets: usize,
    /// Total rows delivered to the sink.
    pub total_rows: u64,
    /// True if `max_rows` was hit and rows were dropped.
    pub truncated: bool,
    /// True if the batch was a rollback-wrapped dry-run
    /// (`BEGIN TRAN; <DML …>; ROLLBACK`). The reported affected-row counts
    /// reflect what *would* have changed; nothing was committed. Lets the UI
    /// label the result as rolled back rather than applied.
    pub rolled_back: bool,
}

/// Result of a connectivity test.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TestReport {
    /// Server version string, if obtained.
    pub server_version: Option<String>,
    /// Round-trip time of the test, in milliseconds.
    pub elapsed_ms: u64,
}

/// Receives streamed result-set events from a driver. The Tauri layer
/// implements this over a `Channel`; tests implement it over a `Vec`.
#[async_trait::async_trait]
pub trait RowSink: Send {
    /// Column metadata for result set `set_index` (a new index ⇒ a new set).
    async fn on_meta(&mut self, set_index: usize, columns: Vec<Column>) -> Flow;
    /// A batch of rows for result set `set_index`.
    async fn on_rows(&mut self, set_index: usize, rows: Vec<Vec<CellValue>>) -> Flow;
    /// Result set `set_index` finished; `affected` is the row count for DML.
    async fn on_set_end(&mut self, set_index: usize, affected: Option<u64>) -> Flow;
}

/// Supplies typed rows to an import — the mirror of [`RowSink`].
///
/// A driver's import path *pulls* batches of rows (already coerced to the
/// destination columns' types and in destination-column order) and inserts them.
/// An empty batch signals the source is exhausted. A returned error aborts the
/// import (and, in a transactional import, triggers a rollback). The CSV
/// implementation lives in [`crate::import::CsvRowSource`].
#[async_trait::async_trait]
pub trait RowSource: Send {
    /// The next batch of rows in destination-column order. An empty `Vec` means
    /// the source is exhausted.
    async fn next_batch(&mut self) -> Result<Vec<Vec<CellValue>>, CoreError>;
}

/// Identifies an existing table to import rows into.
#[derive(Clone, Debug)]
pub struct ImportTarget {
    /// Target database (the connection's current database when `None`).
    pub database: Option<String>,
    /// Target schema (e.g. `dbo`).
    pub schema: String,
    /// Target table name.
    pub table: String,
    /// Destination column names, ordered to match each source row's cells.
    pub columns: Vec<String>,
}

/// One column of a table to be created by [`Connection::create_table`].
#[derive(Clone, Debug)]
pub struct NewColumn {
    /// Column name.
    pub name: String,
    /// A backend DDL type fragment, e.g. `"INT"`, `"NVARCHAR(255)"`,
    /// `"DECIMAL(38,10)"`. Drivers validate this before splicing it into DDL.
    pub sql_type: String,
    /// Whether the column permits `NULL`.
    pub nullable: bool,
}

/// A live connection to a database. Not `Sync`: a connection is driven from one
/// task at a time (the pool hands out exclusive access).
#[async_trait::async_trait]
pub trait Connection: Send {
    /// Execute `sql`, streaming rows to `sink`. Honours `cancel` cooperatively.
    async fn execute(
        &mut self,
        sql: &str,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
        cancel: &CancelToken,
    ) -> Result<ExecOutcome, CoreError>;

    /// List databases on the server.
    async fn list_databases(&mut self) -> Result<Vec<DatabaseInfo>, CoreError>;

    /// List schemas in `database`.
    async fn list_schemas(&mut self, database: &str) -> Result<Vec<SchemaInfo>, CoreError>;

    /// List tables and views in `database`.`schema`.
    async fn list_tables(
        &mut self,
        database: &str,
        schema: &str,
    ) -> Result<Vec<TableInfo>, CoreError>;

    /// List columns of `database`.`schema`.`table`.
    async fn list_columns(
        &mut self,
        database: &str,
        schema: &str,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, CoreError>;

    /// Lightweight liveness check; drives reconnect UX.
    async fn ping(&mut self) -> Result<(), CoreError>;

    /// Return the name of the currently active database for this connection.
    /// Returns an empty string if the driver does not support this concept.
    async fn current_database(&mut self) -> Result<String, CoreError> {
        Ok(String::new())
    }

    /// Switch to `database` as the active database for this connection.
    async fn use_database(&mut self, _database: &str) -> Result<(), CoreError> {
        Err(CoreError::Unsupported(
            "use_database is not supported by this driver".into(),
        ))
    }

    /// Create a new database named `database`. Drivers must bracket/quote the
    /// identifier (the name comes from user input).
    async fn create_database(&mut self, _database: &str) -> Result<(), CoreError> {
        Err(CoreError::Unsupported(
            "create_database is not supported by this driver".into(),
        ))
    }

    /// Drop the database named `database`. Drivers must bracket/quote the
    /// identifier (the name comes from user input). Fails if the database is in
    /// use by other connections.
    async fn drop_database(&mut self, _database: &str) -> Result<(), CoreError> {
        Err(CoreError::Unsupported(
            "drop_database is not supported by this driver".into(),
        ))
    }

    /// Rename a database from `from` to `to`. Drivers must bracket/quote both
    /// identifiers (the names come from user input).
    ///
    /// With `force == false` the rename must **fail fast** rather than block
    /// indefinitely when the database is in use, returning
    /// [`CoreError::DatabaseInUse`] so the caller can offer a forced retry.
    /// With `force == true` the driver forcibly disconnects other sessions to
    /// complete the rename (rolling back their in-flight transactions).
    async fn rename_database(
        &mut self,
        _from: &str,
        _to: &str,
        _force: bool,
    ) -> Result<(), CoreError> {
        Err(CoreError::Unsupported(
            "rename_database is not supported by this driver".into(),
        ))
    }

    /// Bring `database` online (`online = true`) or take it offline
    /// (`online = false`). Taking a database offline terminates all other
    /// connections to it. Drivers must bracket/quote the identifier.
    async fn set_database_online(
        &mut self,
        _database: &str,
        _online: bool,
    ) -> Result<(), CoreError> {
        Err(CoreError::Unsupported(
            "set_database_online is not supported by this driver".into(),
        ))
    }

    /// Create a table from a column spec (for "import as new table"). Drivers
    /// must bracket/quote every identifier and validate each `sql_type`.
    async fn create_table(
        &mut self,
        _database: Option<&str>,
        _schema: &str,
        _table: &str,
        _columns: &[NewColumn],
        _cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        Err(CoreError::Unsupported(
            "create_table is not supported by this driver".into(),
        ))
    }

    /// Drop the table identified by `database`/`schema`/`table`. Drivers must
    /// bracket/quote every identifier (the names come from user input). Used by
    /// the import flow's "replace existing" recovery: after an explicit,
    /// confirmed retry the caller drops the half-created table before
    /// re-running [`create_table`](Self::create_table).
    async fn drop_table(
        &mut self,
        _database: Option<&str>,
        _schema: &str,
        _table: &str,
        _cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        Err(CoreError::Unsupported(
            "drop_table is not supported by this driver".into(),
        ))
    }

    /// Insert rows pulled from `source` into `target` using **bound
    /// parameters** (never spliced values). When `atomic`, the whole import runs
    /// in a transaction and any error rolls it back; otherwise each batch commits
    /// as it lands. Returns the number of rows inserted. `batch_size` is the
    /// caller's desired rows-per-statement (drivers may sub-batch to respect
    /// parameter limits). Honours `cancel` between batches.
    async fn import_rows(
        &mut self,
        _target: &ImportTarget,
        _source: &mut dyn RowSource,
        _atomic: bool,
        _batch_size: usize,
        _cancel: &CancelToken,
    ) -> Result<u64, CoreError> {
        Err(CoreError::Unsupported(
            "import_rows is not supported by this driver".into(),
        ))
    }

    /// Back up `database` to the server-side file `to_path` (a path on the
    /// **database server's** filesystem, not the client's). Honours `cancel`
    /// only cooperatively via the hard-stop the Tauri layer issues (the backup
    /// is a single statement); progress is observed out-of-band by polling
    /// [`backup_percent_complete`](Self::backup_percent_complete). Drivers must
    /// bracket-quote the database name and escape `to_path` as a string literal.
    async fn backup_database(
        &mut self,
        _database: &str,
        _to_path: &str,
        _opts: &BackupOptions,
        _cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        Err(CoreError::Unsupported(
            "backup_database is not supported by this driver".into(),
        ))
    }

    /// List the logical files contained in the backup at `from_path`
    /// (`RESTORE FILELISTONLY`). Used to preview a `.bak` and to plan `MOVE`
    /// relocations for a restore.
    async fn restore_filelist(&mut self, _from_path: &str) -> Result<Vec<BackupFile>, CoreError> {
        Err(CoreError::Unsupported(
            "restore_filelist is not supported by this driver".into(),
        ))
    }

    /// List the current physical files of an existing `database`
    /// (`sys.master_files`), used as relocation targets when restoring over it.
    async fn database_files(&mut self, _database: &str) -> Result<Vec<DbFile>, CoreError> {
        Err(CoreError::Unsupported(
            "database_files is not supported by this driver".into(),
        ))
    }

    /// The server's default data/log directories, used as a fallback when a
    /// restore relocation target cannot be derived from the target database.
    async fn default_file_dirs(&mut self) -> Result<DefaultDirs, CoreError> {
        Err(CoreError::Unsupported(
            "default_file_dirs is not supported by this driver".into(),
        ))
    }

    /// The server's default **backup** directory, for pre-filling a backup
    /// destination and as the starting point for the server-side file browser.
    async fn default_backup_dir(&mut self) -> Result<String, CoreError> {
        Err(CoreError::Unsupported(
            "default_backup_dir is not supported by this driver".into(),
        ))
    }

    /// List the immediate entries (sub-directories and files) of the **server**
    /// directory `path`, so the UI can browse the server's filesystem to pick a
    /// backup destination or a `.bak` to restore. Returns names only.
    async fn list_server_dir(&mut self, _path: &str) -> Result<Vec<ServerDirEntry>, CoreError> {
        Err(CoreError::Unsupported(
            "list_server_dir is not supported by this driver".into(),
        ))
    }

    /// Restore the backup at `from_path` **over** the existing database
    /// `target` (`RESTORE … WITH REPLACE`), relocating each file per `moves`.
    /// The target is taken single-user for the duration and returned to
    /// multi-user afterwards (even on failure). Drivers must bracket-quote the
    /// database name and escape all paths/logical names as string literals.
    async fn restore_database(
        &mut self,
        _target: &str,
        _from_path: &str,
        _moves: &[FileMove],
        _opts: &RestoreOptions,
        _cancel: &CancelToken,
    ) -> Result<(), CoreError> {
        Err(CoreError::Unsupported(
            "restore_database is not supported by this driver".into(),
        ))
    }

    /// The server-assigned session id (`@@SPID`) of this connection, used to
    /// correlate the running backup/restore in `sys.dm_exec_requests`.
    async fn current_session_id(&mut self) -> Result<i32, CoreError> {
        Err(CoreError::Unsupported(
            "current_session_id is not supported by this driver".into(),
        ))
    }

    /// The `percent_complete` of the request running on session `spid`
    /// (`sys.dm_exec_requests`), or `None` if no such request is active. Called
    /// on a *separate* connection while a backup/restore runs. Requires
    /// `VIEW SERVER STATE`; a permission error should surface as an `Err` so the
    /// caller can fall back to indeterminate progress.
    async fn backup_percent_complete(&mut self, _spid: i32) -> Result<Option<f32>, CoreError> {
        Err(CoreError::Unsupported(
            "backup_percent_complete is not supported by this driver".into(),
        ))
    }

    /// Terminate the server session `spid` (`KILL`). Best-effort cancellation of
    /// a backup/restore, issued from a separate connection.
    async fn kill_session(&mut self, _spid: i32) -> Result<(), CoreError> {
        Err(CoreError::Unsupported(
            "kill_session is not supported by this driver".into(),
        ))
    }

    /// Delete a single file on the **server's** filesystem (e.g. a `.bak` after a
    /// restore). Best-effort and may be refused by server policy (the driver must
    /// never relax security settings to do it). Drivers must escape the path.
    async fn delete_server_file(&mut self, _path: &str) -> Result<(), CoreError> {
        Err(CoreError::Unsupported(
            "delete_server_file is not supported by this driver".into(),
        ))
    }
}

/// A database backend: opens connections and advertises its capabilities.
#[async_trait::async_trait]
pub trait DatabaseDriver: Send + Sync {
    /// Which backend this is.
    fn id(&self) -> DriverId;

    /// What this driver supports (drives UI feature gating).
    fn capabilities(&self) -> DriverCapabilities;

    /// Validate connectivity without establishing a pooled session.
    async fn test_connection(
        &self,
        spec: &ConnectionSpec,
        secret: &Secret,
    ) -> Result<TestReport, CoreError>;

    /// Open a live connection.
    async fn connect(
        &self,
        spec: &ConnectionSpec,
        secret: &Secret,
    ) -> Result<Box<dyn Connection>, CoreError>;
}

/// Return the driver implementation for `id`, if compiled into this build.
///
/// New backends are registered here as they are implemented.
pub fn driver_for(id: DriverId) -> Result<Box<dyn DatabaseDriver>, CoreError> {
    match id {
        #[cfg(feature = "mssql")]
        DriverId::Mssql => Ok(Box::new(mssql::MssqlDriver::new())),
        #[cfg(feature = "postgres")]
        DriverId::Postgres => Ok(Box::new(postgres::PostgresDriver::new())),
        #[cfg(feature = "sqlite")]
        DriverId::Sqlite => Ok(Box::new(sqlite::SqliteDriver::new())),
        // MySQL feature-gating is wired (see `shared`), but no driver is
        // registered yet — it falls through to Unsupported until implemented.
        other => Err(CoreError::Unsupported(format!(
            "driver {other:?} is not available in this build"
        ))),
    }
}
