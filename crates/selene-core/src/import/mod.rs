//! Streaming CSV import: parse a CSV file, coerce each field to the destination
//! column's logical type, and feed typed rows to a driver for insertion.
//!
//! ## Mirror of `export/`
//! Where an [`Exporter`](crate::export::Exporter) is a
//! [`RowSink`](crate::driver::RowSink) the driver streams *into*, a
//! [`CsvRowSource`] is a [`RowSource`](crate::driver::RowSource) the driver
//! pulls *from*. Everything in this module is DB-agnostic and unit-testable; the
//! actual `INSERT`/`CREATE TABLE` lives behind the driver trait.
//!
//! ## Type safety
//! CSV fields are raw text. [`coerce_cell`] converts a field to a
//! [`CellValue`] for a destination column's [`LogicalType`], failing loudly when
//! the text cannot represent the target type — that drives the abort-vs-skip
//! policy. For a *new* table whose types are unknown, [`infer_type`] guesses a
//! sensible SQL Server type from a sample of the data.

mod csv;

pub use csv::{CsvRowSource, DestColumn};

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::value::{CellValue, LogicalType, TemporalKind};

/// All user-configurable options for reading and importing a CSV file.
#[derive(Clone, Copy, Debug)]
pub struct CsvImportOptions {
    /// Field separator byte (e.g. `b','`, `b';'`, `b'\t'`, `b'|'`).
    pub delimiter: u8,
    /// Character used to quote fields containing special characters.
    pub quote: u8,
    /// Whether quote characters are interpreted at all. When `false`, quotes are
    /// treated as ordinary data (for files that never quote fields).
    pub quoting: bool,
    /// Whether the first row is a header (column names) rather than data.
    pub has_header: bool,
    /// Treat an empty field as SQL `NULL`. When `false`, an empty field maps to
    /// an empty string for text columns and `NULL` for every other type.
    pub empty_as_null: bool,
    /// Abort the whole import on the first unconvertible row (transactional).
    /// When `false`, bad rows are skipped and counted in [`ImportSummary`].
    pub atomic: bool,
}

impl Default for CsvImportOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            quote: b'"',
            quoting: true,
            has_header: true,
            empty_as_null: true,
            atomic: true,
        }
    }
}

/// Outcome of a completed import.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSummary {
    /// Rows successfully inserted.
    pub rows_inserted: u64,
    /// Rows skipped because a field could not be coerced (skip mode only).
    pub rows_skipped: u64,
}

/// A SQL Server column type inferred from sample data, with the matching
/// logical bucket used for coercion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferredType {
    /// DDL type fragment, e.g. `"INT"`, `"DECIMAL(18,4)"`, `"NVARCHAR(255)"`.
    pub sql_type: String,
    /// The logical bucket coercion uses for this type.
    pub logical: LogicalType,
}

/// Header names, a sample of rows, and a per-column inferred type — everything
/// the mapping menu needs to render. Produced by [`analyze_csv`].
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CsvAnalysis {
    /// Column names (from the header row, or synthesised `column_N` when the
    /// file has no header).
    pub headers: Vec<String>,
    /// Up to `sample_limit` data rows, as raw strings, for a preview table.
    pub sample_rows: Vec<Vec<String>>,
    /// One inferred type per column (aligned with `headers`).
    pub inferred: Vec<InferredType>,
    /// The first few **raw, unparsed** lines of the file (BOM stripped). Lets the
    /// UI show what the file actually looks like — crucially, which delimiter it
    /// uses — independent of the currently-selected parse options.
    pub raw_preview: Vec<String>,
}

/// Read a CSV's header plus up to `sample_limit` data rows and infer a SQL
/// Server type per column.
pub fn analyze_csv(
    path: &std::path::Path,
    opts: &CsvImportOptions,
    sample_limit: usize,
) -> Result<CsvAnalysis, CoreError> {
    csv::analyze(path, opts, sample_limit)
}

/// Coerce a raw CSV field to a [`CellValue`] for a destination column of the
/// given `target` logical type.
///
/// An empty (or whitespace-only) field becomes [`CellValue::Null`] when
/// `empty_as_null`; otherwise it stays an empty string for [`LogicalType::Text`]
/// and is `Null` for every other type (an empty numeric/date cell has no
/// meaningful zero value). Returns [`CoreError::Import`] when the text cannot
/// represent the target type — the caller decides whether that aborts the import
/// or skips the row.
pub fn coerce_cell(
    text: &str,
    target: LogicalType,
    empty_as_null: bool,
) -> Result<CellValue, CoreError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        // For text we preserve the *original* (possibly whitespace) value when
        // the user opted out of empty-as-null; everything else has no zero form.
        return Ok(match target {
            LogicalType::Text if !empty_as_null => CellValue::String(text.to_string()),
            _ => CellValue::Null,
        });
    }

    match target {
        LogicalType::Integer => trimmed
            .parse::<i64>()
            .map(CellValue::I64)
            .map_err(|_| coerce_err(text, "integer")),
        LogicalType::Float => trimmed
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(CellValue::F64)
            .ok_or_else(|| coerce_err(text, "float")),
        LogicalType::Decimal => {
            if is_decimal(trimmed) {
                Ok(CellValue::Decimal(trimmed.to_string()))
            } else {
                Err(coerce_err(text, "decimal"))
            }
        }
        LogicalType::Boolean => parse_bool(trimmed)
            .map(CellValue::Bool)
            .ok_or_else(|| coerce_err(text, "boolean")),
        LogicalType::Date => parse_date(trimmed)
            .map(|iso| CellValue::DateTime {
                iso,
                kind: TemporalKind::Date,
            })
            .ok_or_else(|| coerce_err(text, "date")),
        LogicalType::Time => parse_time(trimmed)
            .map(|iso| CellValue::DateTime {
                iso,
                kind: TemporalKind::Time,
            })
            .ok_or_else(|| coerce_err(text, "time")),
        LogicalType::DateTime => parse_datetime(trimmed)
            .map(|iso| CellValue::DateTime {
                iso,
                kind: TemporalKind::DateTime,
            })
            .ok_or_else(|| coerce_err(text, "datetime")),
        LogicalType::Uuid => {
            if is_uuid(trimmed) {
                Ok(CellValue::Uuid(trimmed.to_string()))
            } else {
                Err(coerce_err(text, "uuid"))
            }
        }
        LogicalType::Binary => parse_hex(trimmed)
            .map(CellValue::Bytes)
            .ok_or_else(|| coerce_err(text, "binary")),
        // Text / Json / Other / Null buckets accept any string verbatim.
        _ => Ok(CellValue::String(text.to_string())),
    }
}

/// Infer a SQL Server column type from a sample of a column's values.
///
/// Walks the non-empty samples once and narrows from most-specific to
/// least-specific: integer → decimal → boolean → date → datetime → text. An
/// all-empty sample (or no samples) defaults to `NVARCHAR(255)`.
pub fn infer_type(samples: &[&str]) -> InferredType {
    let mut seen = false;
    let mut all_int = true;
    let mut int_needs_big = false;
    let mut all_decimal = true;
    let mut all_bool = true;
    let mut all_date = true;
    let mut all_datetime = true;
    let mut max_len = 0usize;

    for raw in samples {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        seen = true;
        max_len = max_len.max(raw.chars().count());

        if all_int {
            match s.parse::<i64>() {
                Ok(n) => {
                    if n < i64::from(i32::MIN) || n > i64::from(i32::MAX) {
                        int_needs_big = true;
                    }
                }
                Err(_) => all_int = false,
            }
        }
        if all_decimal && !is_decimal(s) {
            all_decimal = false;
        }
        if all_bool && parse_bool(s).is_none() {
            all_bool = false;
        }
        if all_date && parse_date(s).is_none() {
            all_date = false;
        }
        if all_datetime && parse_datetime(s).is_none() {
            all_datetime = false;
        }
    }

    if !seen {
        return InferredType {
            sql_type: "NVARCHAR(255)".to_string(),
            logical: LogicalType::Text,
        };
    }
    if all_int {
        let sql_type = if int_needs_big { "BIGINT" } else { "INT" };
        return InferredType {
            sql_type: sql_type.to_string(),
            logical: LogicalType::Integer,
        };
    }
    if all_decimal {
        return InferredType {
            // A wide, exact default covers most financial/numeric CSVs without
            // probing each value's precision; the user can narrow it in the menu.
            sql_type: "DECIMAL(38,10)".to_string(),
            logical: LogicalType::Decimal,
        };
    }
    if all_bool {
        return InferredType {
            sql_type: "BIT".to_string(),
            logical: LogicalType::Boolean,
        };
    }
    // Date must be checked before datetime: a pure `YYYY-MM-DD` also parses as a
    // datetime (midnight), but `DATE` is the tighter, more faithful type.
    if all_date {
        return InferredType {
            sql_type: "DATE".to_string(),
            logical: LogicalType::Date,
        };
    }
    if all_datetime {
        return InferredType {
            sql_type: "DATETIME2".to_string(),
            logical: LogicalType::DateTime,
        };
    }

    let sql_type = match max_len {
        0..=50 => "NVARCHAR(50)".to_string(),
        51..=100 => "NVARCHAR(100)".to_string(),
        101..=255 => "NVARCHAR(255)".to_string(),
        256..=4000 => "NVARCHAR(4000)".to_string(),
        _ => "NVARCHAR(MAX)".to_string(),
    };
    InferredType {
        sql_type,
        logical: LogicalType::Text,
    }
}

/// Map a SQL Server data-type **name** (as reported by introspection, e.g.
/// `"int"`, `"nvarchar"`, `"datetime2"`) to Selene's coarse [`LogicalType`].
///
/// Existing-table targets give us type names (not tiberius `ColumnType`s), so we
/// key off the name. Comparison is case-insensitive and ignores any
/// `(length)`/`(p,s)` suffix the caller may have left attached.
pub fn logical_for_sql_type(name: &str) -> LogicalType {
    let base = name
        .trim()
        .split(['(', ' '])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match base.as_str() {
        "bit" => LogicalType::Boolean,
        "tinyint" | "smallint" | "int" | "bigint" => LogicalType::Integer,
        "real" | "float" => LogicalType::Float,
        "decimal" | "numeric" | "money" | "smallmoney" => LogicalType::Decimal,
        "uniqueidentifier" => LogicalType::Uuid,
        "date" => LogicalType::Date,
        "time" => LogicalType::Time,
        "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" => LogicalType::DateTime,
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion" => LogicalType::Binary,
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" => LogicalType::Text,
        "xml" => LogicalType::Json,
        _ => LogicalType::Other,
    }
}

// --- field predicates / parsers ------------------------------------------

fn coerce_err(text: &str, target: &str) -> CoreError {
    // We include a short, truncated snippet of the offending text so the user
    // can locate the bad cell, but cap it so a huge field never bloats the
    // message (and we never log this above DEBUG anyway).
    let snippet: String = text.chars().take(40).collect();
    CoreError::Import(format!("value {snippet:?} is not a valid {target}"))
}

/// A strict decimal: optional sign, digits with at most one dot, at least one
/// digit overall. No scientific notation, no thousands separators.
fn is_decimal(s: &str) -> bool {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    if body.is_empty() {
        return false;
    }
    let mut dots = 0;
    let mut digits = 0;
    for ch in body.chars() {
        match ch {
            '.' => {
                dots += 1;
                if dots > 1 {
                    return false;
                }
            }
            c if c.is_ascii_digit() => digits += 1,
            _ => return false,
        }
    }
    digits > 0
}

/// Parse a boolean token (case-insensitive): `true/false`, `t/f`, `yes/no`,
/// `y/n`, `1/0`.
fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" | "1" => Some(true),
        "false" | "f" | "no" | "n" | "0" => Some(false),
        _ => None,
    }
}

/// Parse an ISO-ish date (`YYYY-MM-DD` or `YYYY/MM/DD`) → `%Y-%m-%d`.
fn parse_date(s: &str) -> Option<String> {
    for fmt in ["%Y-%m-%d", "%Y/%m/%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Some(d.format("%Y-%m-%d").to_string());
        }
    }
    None
}

/// Parse a time (`HH:MM[:SS[.fff]]`) → `%H:%M:%S%.f`.
fn parse_time(s: &str) -> Option<String> {
    for fmt in ["%H:%M:%S%.f", "%H:%M:%S", "%H:%M"] {
        if let Ok(t) = NaiveTime::parse_from_str(s, fmt) {
            return Some(t.format("%H:%M:%S%.f").to_string());
        }
    }
    None
}

/// Parse a datetime with a `T` or space separator (date-only accepted, taken as
/// midnight) → `%Y-%m-%dT%H:%M:%S%.f`.
fn parse_datetime(s: &str) -> Option<String> {
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.format("%Y-%m-%dT%H:%M:%S%.f").to_string());
        }
    }
    // A bare date is a valid datetime at midnight.
    parse_date(s).map(|d| format!("{d}T00:00:00"))
}

/// Validate a UUID in canonical 8-4-4-4-12 hyphenated form (case-insensitive),
/// optionally wrapped in braces. Returns true if well-formed.
fn is_uuid(s: &str) -> bool {
    let s = s
        .strip_prefix('{')
        .and_then(|r| r.strip_suffix('}'))
        .unwrap_or(s);
    let groups = [8usize, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != groups.len() {
        return false;
    }
    parts
        .iter()
        .zip(groups)
        .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Parse a hex string (optional `0x` prefix, even digit count) into bytes.
fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let body = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if body.is_empty() || body.len() % 2 != 0 || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() / 2);
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_integer() {
        assert_eq!(
            coerce_cell("42", LogicalType::Integer, true).unwrap(),
            CellValue::I64(42)
        );
        assert_eq!(
            coerce_cell(" -7 ", LogicalType::Integer, true).unwrap(),
            CellValue::I64(-7)
        );
        assert!(coerce_cell("abc", LogicalType::Integer, true).is_err());
        assert!(coerce_cell("1.5", LogicalType::Integer, true).is_err());
    }

    #[test]
    fn coerce_decimal_is_lossless_text() {
        assert_eq!(
            coerce_cell("12345.67890", LogicalType::Decimal, true).unwrap(),
            CellValue::Decimal("12345.67890".to_string())
        );
        assert!(coerce_cell("1,5", LogicalType::Decimal, true).is_err());
        assert!(coerce_cell("1.2.3", LogicalType::Decimal, true).is_err());
    }

    #[test]
    fn coerce_boolean_tokens() {
        for t in ["true", "T", "Yes", "1", "y"] {
            assert_eq!(
                coerce_cell(t, LogicalType::Boolean, true).unwrap(),
                CellValue::Bool(true)
            );
        }
        for f in ["false", "f", "No", "0", "N"] {
            assert_eq!(
                coerce_cell(f, LogicalType::Boolean, true).unwrap(),
                CellValue::Bool(false)
            );
        }
        assert!(coerce_cell("maybe", LogicalType::Boolean, true).is_err());
    }

    #[test]
    fn coerce_dates_and_datetimes() {
        assert_eq!(
            coerce_cell("2026-06-19", LogicalType::Date, true).unwrap(),
            CellValue::DateTime {
                iso: "2026-06-19".into(),
                kind: TemporalKind::Date
            }
        );
        assert_eq!(
            coerce_cell("2026-06-19 12:30:00", LogicalType::DateTime, true).unwrap(),
            CellValue::DateTime {
                iso: "2026-06-19T12:30:00".into(),
                kind: TemporalKind::DateTime
            }
        );
        // A bare date is a valid datetime at midnight.
        assert_eq!(
            coerce_cell("2026-06-19", LogicalType::DateTime, true).unwrap(),
            CellValue::DateTime {
                iso: "2026-06-19T00:00:00".into(),
                kind: TemporalKind::DateTime
            }
        );
        assert!(coerce_cell("19-06-2026", LogicalType::Date, true).is_err());
    }

    #[test]
    fn coerce_empty_respects_null_policy() {
        assert_eq!(
            coerce_cell("", LogicalType::Integer, true).unwrap(),
            CellValue::Null
        );
        // empty_as_null = false keeps an empty string for text only.
        assert_eq!(
            coerce_cell("", LogicalType::Text, false).unwrap(),
            CellValue::String(String::new())
        );
        assert_eq!(
            coerce_cell("", LogicalType::Integer, false).unwrap(),
            CellValue::Null
        );
    }

    #[test]
    fn coerce_uuid_and_binary() {
        assert_eq!(
            coerce_cell(
                "936DA01F-9ABD-4D9D-80C7-02AF85C822A8",
                LogicalType::Uuid,
                true
            )
            .unwrap(),
            CellValue::Uuid("936DA01F-9ABD-4D9D-80C7-02AF85C822A8".to_string())
        );
        assert!(coerce_cell("not-a-uuid", LogicalType::Uuid, true).is_err());
        assert_eq!(
            coerce_cell("0xDEad", LogicalType::Binary, true).unwrap(),
            CellValue::Bytes(vec![0xDE, 0xAD])
        );
        assert!(coerce_cell("0xABC", LogicalType::Binary, true).is_err());
    }

    #[test]
    fn infer_picks_tightest_type() {
        assert_eq!(infer_type(&["1", "2", "3"]).sql_type, "INT");
        assert_eq!(infer_type(&["1", "9999999999"]).sql_type, "BIGINT");
        assert_eq!(
            infer_type(&["1.5", "2", "-0.25"]).sql_type,
            "DECIMAL(38,10)"
        );
        assert_eq!(infer_type(&["true", "false", "1"]).sql_type, "BIT");
        assert_eq!(infer_type(&["2026-06-19", "2020-01-01"]).sql_type, "DATE");
        assert_eq!(
            infer_type(&["2026-06-19 12:00:00", "2020-01-01T00:00:00"]).sql_type,
            "DATETIME2"
        );
        assert_eq!(infer_type(&["hello", "world"]).sql_type, "NVARCHAR(50)");
        assert_eq!(infer_type(&[]).sql_type, "NVARCHAR(255)");
        // A mix of int and text falls back to text.
        assert_eq!(infer_type(&["1", "two"]).logical, LogicalType::Text);
    }

    #[test]
    fn logical_for_sql_type_maps_names() {
        assert_eq!(logical_for_sql_type("int"), LogicalType::Integer);
        assert_eq!(logical_for_sql_type("NVARCHAR(255)"), LogicalType::Text);
        assert_eq!(logical_for_sql_type("decimal(18, 4)"), LogicalType::Decimal);
        assert_eq!(logical_for_sql_type("datetime2"), LogicalType::DateTime);
        assert_eq!(logical_for_sql_type("uniqueidentifier"), LogicalType::Uuid);
        assert_eq!(logical_for_sql_type("bit"), LogicalType::Boolean);
        assert_eq!(logical_for_sql_type("geography"), LogicalType::Other);
    }
}
