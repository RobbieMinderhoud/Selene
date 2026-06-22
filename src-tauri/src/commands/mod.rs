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
pub mod connection;
pub mod export;
pub mod fs;
pub mod health;
pub mod import;
pub mod introspect;
pub mod query;
pub mod session;

use serde::Serialize;

use selene_core::{CellValue, Column, ExecOutcome};

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
