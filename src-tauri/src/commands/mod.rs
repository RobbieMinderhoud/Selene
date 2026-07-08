//! The Tauri IPC command surface.
//!
//! This module is a **thin adapter**: it (de)serializes arguments, pulls live
//! state out of [`AppState`](crate::state::AppState), and delegates every piece
//! of real work to `selene-core`. No SQL, driver, export, or guard logic is
//! reimplemented here.
//!
//! ## Logging discipline
//! Commands instrument with `tracing` at `INFO` for lifecycle events
//! (connect/disconnect, query start/finish, export). They **never** log SQL
//! text, row/cell data, file paths' contents, or anything secret above
//! `DEBUG`/`TRACE`; secrets are wrapped in [`Secret`](selene_core::Secret),
//! which cannot be `Display`ed or serialized at all. A query's id and row
//! counts are safe to log; its `sql` is not (it can embed literal values).
//!
//! ## Wire shapes
//! The frontend hand-writes TypeScript types matching the structs here (ts-rs
//! is deferred to a later phase). Field names are `camelCase` on the wire via
//! `#[serde(rename_all = "camelCase")]`; tagged enums carry a `kind`
//! discriminant.

// Submodules are `pub` (not re-exported) so `generate_handler!` can reference
// each command at its real path (e.g. `commands::connection::connections_list`).
// The `tauri::command` macro emits sibling helper items next to each function;
// a `pub use` re-export does not bring those helpers along, so the macro must
// see the function at its defining path.
pub mod backup;
pub mod connection;
pub mod export;
pub mod fs;
pub mod health;
pub mod import;
pub mod introspect;
pub mod multi;
pub mod query;
pub mod session;

use serde::Serialize;

use selene_core::{CellValue, Column, ExecOutcome};

/// First byte of an optional frontend string, or `fallback` when absent/empty.
/// Turns a delimiter/quote option into the single byte the core CSV options
/// expect. Shared by the export and import commands.
pub(crate) fn first_byte(s: Option<&str>, fallback: u8) -> u8 {
    s.and_then(|v| v.as_bytes().first().copied())
        .unwrap_or(fallback)
}

/// Streaming events emitted by [`query_run`] over a `tauri::ipc::Channel`.
///
/// Internally tagged with a `kind` field; every field is `camelCase` on the
/// wire. The lifecycle for a successful run is:
/// `Started` → (`Meta`, `Rows`*, `SetEnd`)+ → `Finished`. A run can instead end
/// in `Cancelled` (cooperative cancel won the race) or `Failed`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum QueryEvent {
    /// The query was accepted and a task spawned. Carries the id the frontend
    /// uses to cancel.
    #[serde(rename_all = "camelCase")]
    Started { query_id: String },
    /// Column metadata for result set `set_index`. A new index marks a new set.
    #[serde(rename_all = "camelCase")]
    Meta {
        set_index: usize,
        columns: Vec<Column>,
    },
    /// A batch of rows for result set `set_index`. Cell values use the core's
    /// tagged [`CellValue`] representation (`{ "t": ..., "v": ... }`).
    #[serde(rename_all = "camelCase")]
    Rows {
        set_index: usize,
        rows: Vec<Vec<CellValue>>,
    },
    /// Result set `set_index` finished; `affected` is the row count for DML.
    #[serde(rename_all = "camelCase")]
    SetEnd {
        set_index: usize,
        affected: Option<u64>,
    },
    /// The whole batch completed successfully.
    #[serde(rename_all = "camelCase")]
    Finished {
        outcome: ExecOutcome,
        elapsed_ms: u64,
    },
    /// The query was cancelled (cooperatively) before completing.
    Cancelled,
    /// The query failed. `message` is sanitized and secret-free.
    Failed { message: String },
}

/// Streaming progress events emitted by [`export_result`] over a
/// `tauri::ipc::Channel`. Internally tagged (`kind`), `camelCase` fields.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ExportEvent {
    /// Periodic progress: total rows written to the file so far.
    Progress { rows: u64 },
    /// The export finished successfully; `rows` is the final written count.
    Done { rows: u64 },
    /// The export failed. `message` is sanitized and secret-free.
    Failed { message: String },
}

/// Streaming progress events emitted by
/// [`database_backup`](backup::database_backup) over a `tauri::ipc::Channel`.
/// Internally tagged (`kind`), `camelCase` fields.
///
/// Lifecycle: `Started` → `Progress`* → (`Done` | `Cancelled` | `Failed`).
/// `Progress.percent` is the server's `percent_complete` (0–100), observed by
/// polling a *second* connection; it may not appear at all (e.g. the polling
/// connection lacks `VIEW SERVER STATE`), in which case the UI shows
/// indeterminate progress.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BackupEvent {
    /// The backup was accepted; carries the id used to cancel it.
    #[serde(rename_all = "camelCase")]
    Started { operation_id: String },
    /// Server-reported completion percentage (0–100).
    Progress { percent: f32 },
    /// The backup finished successfully.
    Done,
    /// The backup was cancelled; the underlying session connection was dropped.
    Cancelled,
    /// The backup failed. `message` is sanitized and secret-free.
    Failed { message: String },
}

/// Streaming progress events emitted by
/// [`database_restore`](backup::database_restore) over a `tauri::ipc::Channel`.
/// Same shape and semantics as [`BackupEvent`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RestoreEvent {
    /// The restore was accepted; carries the id used to cancel it.
    #[serde(rename_all = "camelCase")]
    Started { operation_id: String },
    /// Server-reported completion percentage (0–100).
    Progress { percent: f32 },
    /// The restore finished successfully.
    Done,
    /// The restore was cancelled; the database may be left in a restoring state.
    Cancelled,
    /// The restore failed. `message` is sanitized and secret-free.
    Failed { message: String },
}

/// Streaming progress events emitted by
/// [`import_csv`](import::import_csv) over a `tauri::ipc::Channel`. Internally
/// tagged (`kind`), `camelCase` fields.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ImportEvent {
    /// Periodic progress: total rows read from the CSV so far.
    Progress { rows: u64 },
    /// The import finished successfully: `inserted` rows committed, `skipped`
    /// rows dropped due to coercion failures (skip mode only).
    Done { inserted: u64, skipped: u64 },
    /// The import failed. `message` is sanitized and secret-free.
    Failed { message: String },
}

/// Streaming events emitted by [`multi::multi_target_run`](crate::commands::multi::multi_target_run)
/// over a `tauri::ipc::Channel`. Internally tagged (`kind`), `camelCase` fields.
///
/// `execute` mode: `Started` → (`Target`, `TargetDone`)* / `ServerError`* →
/// `Finished`. `results` mode additionally emits a single `Meta` (the unified
/// columns, with `_server`/`_database` prepended) and `Rows` batches as data
/// arrives across targets. A run can instead end in `Cancelled`. If the failure
/// rate crosses the configured threshold the run emits a single `Paused` and
/// idles until resumed (more events) or cancelled.
///
/// Server names, database names, and row counts are safe to put on the wire;
/// they are identifiers and aggregate metrics, never row/cell data or secrets.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MultiEvent {
    /// The run was accepted; `total` is the planned (server, database) count.
    #[serde(rename_all = "camelCase")]
    Started { run_id: String, total: usize },
    /// About to run on one (server, database). `index` is the 1-based running
    /// position; with parallel servers these may not arrive in order.
    #[serde(rename_all = "camelCase")]
    Target {
        connection_id: String,
        server: String,
        database: String,
        index: usize,
        total: usize,
    },
    /// The unified result columns (results mode, emitted once): `_server`,
    /// `_database`, then the first target's columns.
    Meta { columns: Vec<Column> },
    /// A batch of aggregated rows (results mode), each already prefixed with the
    /// `_server` and `_database` cells.
    Rows { rows: Vec<Vec<CellValue>> },
    /// One (server, database) finished. `rows` is the rows *returned* (results
    /// mode) or the rows *affected* (execute mode, when a DML statement reported
    /// a count; `null` for DDL/SELECT with no row-count). `error` is a sanitized
    /// message on failure.
    #[serde(rename_all = "camelCase")]
    TargetDone {
        connection_id: String,
        server: String,
        database: String,
        index: usize,
        rows: Option<u64>,
        error: Option<String>,
    },
    /// A whole server was skipped (no stored password, connect failure, or the
    /// guard blocked it). Its databases are counted as failed.
    #[serde(rename_all = "camelCase")]
    ServerError {
        connection_id: String,
        server: String,
        error: String,
    },
    /// The run auto-paused after the failure rate crossed the configured
    /// threshold; it idles until `multi_target_resume` (continue) or
    /// `multi_target_cancel` (stop). Emitted at most once per run. `failed` and
    /// `total` are database counts (for the prompt's "N of M" wording).
    #[serde(rename_all = "camelCase")]
    Paused { failed: usize, total: usize },
    /// The run completed. Counts are over databases, not servers.
    #[serde(rename_all = "camelCase")]
    Finished {
        succeeded: usize,
        failed: usize,
        rows_total: u64,
    },
    /// The run was cancelled cooperatively.
    Cancelled,
}
