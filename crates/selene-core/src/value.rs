//! Driver-neutral representations of result-set columns and cell values.
//!
//! Every driver maps its native types onto these, so the IPC layer, the data
//! grid, and the exporters all speak one vocabulary. The set is intentionally
//! narrow; backend-specific types are mapped onto the closest variant, and
//! anything unmodelled falls back to [`CellValue::Unsupported`] so the pipeline
//! never fails on an exotic type.

use serde::{Deserialize, Serialize};

/// A single cell value, tagged for a discriminated-union representation on the
/// wire (`{ "t": "I64", "v": 42 }`).
///
/// `Decimal` is kept as a string rather than `f64` so exact numerics
/// (`DECIMAL`/`NUMERIC`/`MONEY`) never lose precision — important for financial
/// data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum CellValue {
    /// SQL `NULL`.
    Null,
    /// Boolean / `bit`.
    Bool(bool),
    /// Signed integer (`tinyint`..`bigint`).
    I64(i64),
    /// Approximate numeric (`float`/`real`).
    F64(f64),
    /// Exact numeric kept as text to preserve precision.
    Decimal(String),
    /// Character data of any width.
    String(String),
    /// Binary data. Rendered as hex by exporters; the IPC layer may re-encode.
    Bytes(Vec<u8>),
    /// Temporal value as an ISO-8601 string plus a kind tag.
    DateTime { iso: String, kind: TemporalKind },
    /// `uniqueidentifier` / UUID, canonical string form.
    Uuid(String),
    /// A type Selene does not model yet, preserved losslessly as text.
    Unsupported { type_name: String, text: String },
}

impl CellValue {
    /// Whether this value is SQL `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, CellValue::Null)
    }
}

/// Which flavour of temporal value a [`CellValue::DateTime`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemporalKind {
    Date,
    Time,
    DateTime,
    DateTimeOffset,
}

/// A coarse, driver-neutral bucket used by the UI for column alignment and
/// default formatting. The exact backend type name is kept separately in
/// [`Column::db_type`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalType {
    Null,
    Boolean,
    Integer,
    Float,
    Decimal,
    Text,
    Binary,
    Date,
    Time,
    DateTime,
    Uuid,
    Json,
    Other,
}

/// Metadata for one result-set column.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// Column name as reported by the server.
    pub name: String,
    /// Zero-based position in the row.
    pub ordinal: usize,
    /// Raw backend type name (e.g. `"nvarchar"`).
    pub db_type: String,
    /// Coarse logical bucket for the UI.
    pub logical: LogicalType,
    /// Whether the column is nullable, if known.
    pub nullable: Option<bool>,
}
