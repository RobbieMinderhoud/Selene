//! `selene-core` — the UI-agnostic data layer for the Selene SQL editor.
//!
//! This crate owns the driver abstraction, query execution, schema
//! introspection, data export, the SQL safety guard, and secret storage —
//! with **zero dependency on the UI shell** (Tauri). Keeping it independent
//! means the data layer is testable with plain `cargo test` and reusable by a
//! future CLI or alternative frontend.

#![forbid(unsafe_code)]

pub mod backup;
pub mod capabilities;
pub mod connection_spec;
pub mod driver;
pub mod error;
pub mod export;
pub mod guard;
pub mod import;
pub mod introspect;
pub mod secret;
pub mod secrets;
pub mod value;

pub use backup::{
    plan_moves, BackupFile, BackupOptions, DbFile, DefaultDirs, FileMove, RestoreOptions,
    ServerDirEntry,
};
pub use capabilities::DriverCapabilities;
pub use connection_spec::{AuthMethod, ConnectionSpec, DriverId, TlsConfig};
pub use driver::{
    driver_for, CancelToken, Connection, DatabaseDriver, ExecOptions, ExecOutcome, Flow,
    ImportTarget, NewColumn, RowSink, RowSource, TestReport,
};
pub use error::CoreError;
pub use export::{
    cell_to_text, CsvLineEnding, CsvOptions, CsvQuoteStyle, ExportFormat, ExportSummary, Exporter,
};
pub use guard::{classify, classify_for, classify_mongo, GuardLevel, GuardVerdict};
pub use import::{
    analyze_csv, coerce_cell, infer_type, logical_for_sql_type, CsvAnalysis, CsvImportOptions,
    CsvRowSource, DestColumn, ImportSummary, InferredType,
};
pub use introspect::{ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo, TableKind};
pub use secret::Secret;
pub use secrets::{KeychainStore, KEYCHAIN_SERVICE};
pub use value::{CellValue, Column, LogicalType, TemporalKind};

/// The crate (and application) version, surfaced for diagnostics and the
/// in-app about screen.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
