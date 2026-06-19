//! Per-driver capability flags.
//!
//! The frontend reads these once on connect and hides or disables features a
//! driver doesn't support (e.g. no "schemas" level for SQLite, no cancel button
//! when server-side cancellation is unavailable). This is the mechanism that
//! keeps the UI honest as new drivers are added.

use serde::{Deserialize, Serialize};

/// What a given driver can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverCapabilities {
    /// Supports a schema level between database and table (MSSQL, Postgres).
    pub schemas: bool,
    /// A single batch can return multiple result sets.
    pub multiple_result_sets: bool,
    /// Running queries can be cancelled server-side.
    pub server_side_cancel: bool,
    /// Supports explicit transactions (begin/commit/rollback).
    pub transactions: bool,
    /// Can produce a query/execution plan.
    pub explain_plan: bool,
    /// Streams rows incrementally rather than buffering the whole set.
    pub streaming_rows: bool,
    /// Has a notion of multiple databases on one server.
    pub list_databases: bool,
    /// Supports editing result-set data back to the source.
    pub data_editing: bool,
}

impl DriverCapabilities {
    /// All capabilities disabled — a base to build a specific driver's set from.
    pub const NONE: Self = Self {
        schemas: false,
        multiple_result_sets: false,
        server_side_cancel: false,
        transactions: false,
        explain_plan: false,
        streaming_rows: false,
        list_databases: false,
        data_editing: false,
    };
}
