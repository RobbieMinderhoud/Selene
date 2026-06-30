//! Conversions between Postgres (sqlx) values and Selene's driver-neutral
//! [`CellValue`] / [`Column`] vocabulary.
//!
//! ## Postgres is statically typed
//! Unlike SQLite, every Postgres column has a fixed type known from the result
//! metadata. So both paths key off the column's type **name** (sqlx reports the
//! canonical Postgres name via `type_info().name()`, e.g. `INT4`, `NUMERIC`,
//! `TIMESTAMPTZ`, `JSONB`). The value path decodes each cell as `Option<T>` for
//! the mapped Rust type `T`:
//! - a `None` (SQL `NULL`) becomes [`CellValue::Null`];
//! - a decode failure degrades to [`CellValue::Unsupported`] carrying the type
//!   name (never a panic, never aborting the result set) — this is what catches
//!   arrays, enums, composites, ranges, and any type Selene does not model.
//!
//! Exact numerics decode to [`rust_decimal::Decimal`] and are rendered losslessly
//! (no `f64`); temporal values decode to `chrono` types and are formatted to
//! ISO-8601 / RFC-3339 by the shared [`convert`](crate::driver::shared::convert)
//! formatters, so every backend reports one date format.

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::types::Uuid;
use sqlx::{Column as _, Row as _, TypeInfo as _};

use crate::driver::shared::convert::{
    decimal_to_string, iso_date, iso_naive_dt, iso_offset_dt, iso_time,
};
use crate::value::{CellValue, Column, LogicalType, TemporalKind};

/// Build Selene column metadata from a Postgres result row's columns.
///
/// `db_type` is the lowercased conventional Postgres type name; `logical` is
/// bucketed from it. JSON/JSONB columns are tagged [`LogicalType::Json`] so the
/// UI formats them as documents. Result columns carry no nullability flag, so
/// `nullable` is `None` (the introspection path fills that in).
pub(crate) fn columns_of(row: &PgRow) -> Vec<Column> {
    row.columns()
        .iter()
        .map(|c| {
            let raw_name = c.type_info().name();
            Column {
                name: c.name().to_string(),
                ordinal: c.ordinal(),
                db_type: raw_name.to_ascii_lowercase(),
                logical: logical_for(raw_name),
                nullable: None,
            }
        })
        .collect()
}

/// Convert one Postgres row into a vector of neutral cell values, one per column.
pub(crate) fn convert_row(row: &PgRow) -> Vec<CellValue> {
    (0..row.len()).map(|i| cell_at(row, i)).collect()
}

/// Convert the cell at column index `i`, dispatching on the column's Postgres
/// type name.
fn cell_at(row: &PgRow, i: usize) -> CellValue {
    // The column's static type name (uppercase canonical form from sqlx, e.g.
    // "INT4", "TIMESTAMPTZ", "_INT4" for an int array).
    let type_name = match row.columns().get(i) {
        Some(c) => c.type_info().name(),
        // Index is in-bounds (0..row.len()); a missing column is unexpected.
        None => return CellValue::Null,
    };

    match type_name {
        "BOOL" => get_map(row, i, type_name, |v: bool| CellValue::Bool(v)),

        "INT2" => get_map(row, i, type_name, |v: i16| CellValue::I64(v as i64)),
        "INT4" => get_map(row, i, type_name, |v: i32| CellValue::I64(v as i64)),
        "INT8" => get_map(row, i, type_name, |v: i64| CellValue::I64(v)),

        "FLOAT4" => get_map(row, i, type_name, |v: f32| CellValue::F64(v as f64)),
        "FLOAT8" => get_map(row, i, type_name, |v: f64| CellValue::F64(v)),

        // Exact numeric: decode to rust_decimal and render losslessly. A value
        // outside rust_decimal's range (Postgres NUMERIC allows far more digits)
        // fails the decode and falls back to text, then to Unsupported.
        "NUMERIC" => decimal_cell(row, i, type_name),
        // MONEY decodes as rust_decimal under sqlx's `rust_decimal` feature; if
        // that fails, fall back to a string, then Unsupported.
        "MONEY" => money_cell(row, i, type_name),

        "UUID" => get_map(row, i, type_name, |v: Uuid| CellValue::Uuid(v.to_string())),

        // Timezone-aware timestamp → RFC-3339 (offset preserved).
        "TIMESTAMPTZ" => get_map(row, i, type_name, |v: DateTime<Utc>| CellValue::DateTime {
            iso: iso_offset_dt(&v),
            kind: TemporalKind::DateTimeOffset,
        }),
        "TIMESTAMP" => get_map(row, i, type_name, |v: NaiveDateTime| CellValue::DateTime {
            iso: iso_naive_dt(&v),
            kind: TemporalKind::DateTime,
        }),
        "DATE" => get_map(row, i, type_name, |v: NaiveDate| CellValue::DateTime {
            iso: iso_date(&v),
            kind: TemporalKind::Date,
        }),
        "TIME" => get_map(row, i, type_name, |v: NaiveTime| CellValue::DateTime {
            iso: iso_time(&v),
            kind: TemporalKind::Time,
        }),

        // JSON/JSONB: decode the document and render its canonical text.
        "JSON" | "JSONB" => get_map(row, i, type_name, |v: serde_json::Value| {
            CellValue::String(v.to_string())
        }),

        "BYTEA" => get_map(row, i, type_name, |v: Vec<u8>| CellValue::Bytes(v)),

        // Character families.
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" => {
            get_map(row, i, type_name, CellValue::String)
        }

        // Arrays (`_INT4`, `TEXT[]`, …), enums, composites, ranges, and any other
        // type Selene does not model: preserve the type name losslessly. We do
        // not attempt a text decode here — many of these have no `Decode<String>`
        // impl, and a failed decode would only yield the same Unsupported value.
        _ => unsupported(type_name),
    }
}

/// Decode column `i` as `Option<T>` and map the present value through `f`.
///
/// `None` (SQL NULL) → [`CellValue::Null`]; a decode error → [`CellValue::Unsupported`]
/// carrying the type name (so one odd cell never aborts the set).
fn get_map<T>(row: &PgRow, i: usize, type_name: &str, f: impl FnOnce(T) -> CellValue) -> CellValue
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    match row.try_get::<Option<T>, _>(i) {
        Ok(Some(v)) => f(v),
        Ok(None) => CellValue::Null,
        Err(_) => unsupported(type_name),
    }
}

/// Decode a `NUMERIC` cell to a lossless decimal string, degrading to a string
/// decode and finally [`CellValue::Unsupported`] on failure (e.g. a value with
/// more digits than `rust_decimal` can hold).
fn decimal_cell(row: &PgRow, i: usize, type_name: &str) -> CellValue {
    match row.try_get::<Option<rust_decimal::Decimal>, _>(i) {
        Ok(Some(v)) => CellValue::Decimal(decimal_to_string(v)),
        Ok(None) => CellValue::Null,
        // Out-of-range for rust_decimal: keep the exact text if Postgres will
        // hand us the textual form, else flag it Unsupported.
        Err(_) => match row.try_get::<Option<String>, _>(i) {
            Ok(Some(s)) => CellValue::Decimal(s),
            Ok(None) => CellValue::Null,
            Err(_) => unsupported(type_name),
        },
    }
}

/// Decode a `MONEY` cell. Postgres `money` decodes as `rust_decimal::Decimal`
/// under sqlx's `rust_decimal` feature; on failure, try a string, then flag it.
fn money_cell(row: &PgRow, i: usize, type_name: &str) -> CellValue {
    match row.try_get::<Option<rust_decimal::Decimal>, _>(i) {
        Ok(Some(v)) => CellValue::Decimal(decimal_to_string(v)),
        Ok(None) => CellValue::Null,
        Err(_) => match row.try_get::<Option<String>, _>(i) {
            Ok(Some(s)) => CellValue::Decimal(s),
            Ok(None) => CellValue::Null,
            Err(_) => unsupported(type_name),
        },
    }
}

/// An [`CellValue::Unsupported`] carrying the (lowercased) Postgres type name and
/// empty text — the type name is enough for the UI to flag the cell, and we keep
/// potentially large/binary payloads out of the value.
fn unsupported(type_name: &str) -> CellValue {
    CellValue::Unsupported {
        type_name: type_name.to_ascii_lowercase(),
        text: String::new(),
    }
}

/// Bucket a Postgres type name into Selene's coarse [`LogicalType`] for UI
/// alignment/formatting. Unknown names (arrays, enums, …) fall through to
/// [`LogicalType::Other`].
fn logical_for(type_name: &str) -> LogicalType {
    match type_name {
        "BOOL" => LogicalType::Boolean,
        "INT2" | "INT4" | "INT8" => LogicalType::Integer,
        "FLOAT4" | "FLOAT8" => LogicalType::Float,
        // MONEY is an exact 2-decimal numeric; bucket it with the exact numerics.
        "NUMERIC" | "MONEY" => LogicalType::Decimal,
        "UUID" => LogicalType::Uuid,
        "TIMESTAMPTZ" | "TIMESTAMP" => LogicalType::DateTime,
        "DATE" => LogicalType::Date,
        "TIME" => LogicalType::Time,
        "JSON" | "JSONB" => LogicalType::Json,
        "BYTEA" => LogicalType::Binary,
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" => LogicalType::Text,
        _ => LogicalType::Other,
    }
}

/// Bind a neutral [`CellValue`] onto a Postgres query for the import write path,
/// mirroring the read path's vocabulary.
///
/// ## Why this binds *native* types, not text
/// Unlike SQL Server (and SQLite), Postgres does **not** implicitly coerce a
/// bound `text` parameter into a different column type on `INSERT`: binding a
/// string into a `numeric`/`timestamptz`/`uuid` column raises
/// `42804 datatype_mismatch`. So decimals, datetimes, and UUIDs are parsed back
/// into their native Rust types ([`rust_decimal::Decimal`], `chrono`, [`Uuid`])
/// and bound as those, which Postgres binary-encodes for the matching column
/// type. Native scalars (int/float/bool/bytea) bind directly. Plain strings bind
/// as text (the destination is a character column). If a parse fails — a value
/// the read path produced as, say, `Decimal` but which is not re-parseable — we
/// fall back to binding the text, so Postgres surfaces a clear coercion error
/// rather than the driver silently dropping data.
///
/// `NULL` binds as a typed `None`. The CSV import coerces every cell of a column
/// to that column's logical type, so a column never mixes a native value in one
/// row with a NULL of an incompatible type in another in a way Postgres rejects.
pub(crate) fn bind_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, PgArguments>,
    value: &CellValue,
) -> sqlx::query::Query<'q, sqlx::Postgres, PgArguments> {
    match value {
        CellValue::Null => query.bind(None::<String>),
        CellValue::Bool(b) => query.bind(*b),
        CellValue::I64(n) => query.bind(*n),
        CellValue::F64(f) => query.bind(*f),
        CellValue::Bytes(b) => query.bind(b.clone()),
        CellValue::String(s) => query.bind(s.clone()),
        // Exact numeric: parse into rust_decimal so Postgres accepts it for a
        // numeric/decimal column. On a parse failure (out of rust_decimal range,
        // or non-numeric text) bind the text and let Postgres report the mismatch.
        CellValue::Decimal(s) => match rust_decimal::Decimal::from_str_exact(s) {
            Ok(d) => query.bind(d),
            Err(_) => query.bind(s.clone()),
        },
        CellValue::Uuid(s) => match Uuid::parse_str(s) {
            Ok(u) => query.bind(u),
            Err(_) => query.bind(s.clone()),
        },
        // Temporal values: parse the ISO/RFC-3339 text back into the chrono type
        // matching the temporal kind, then bind native. A parse failure falls
        // back to text (Postgres then reports the coercion error).
        CellValue::DateTime { iso, kind } => bind_datetime(query, iso, *kind),
        CellValue::Unsupported { text, .. } => query.bind(text.clone()),
    }
}

/// Bind a temporal [`CellValue::DateTime`] by parsing its ISO/RFC-3339 text into
/// the chrono type matching `kind` and binding that native value. Falls back to
/// binding the text if the parse fails.
fn bind_datetime<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, PgArguments>,
    iso: &str,
    kind: TemporalKind,
) -> sqlx::query::Query<'q, sqlx::Postgres, PgArguments> {
    match kind {
        // `%.f` matches the optional fractional second emitted by the read path.
        TemporalKind::DateTime => {
            match NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S%.f") {
                Ok(dt) => query.bind(dt),
                Err(_) => query.bind(iso.to_string()),
            }
        }
        TemporalKind::DateTimeOffset => match DateTime::parse_from_rfc3339(iso) {
            Ok(dt) => query.bind(dt.with_timezone(&Utc)),
            Err(_) => query.bind(iso.to_string()),
        },
        TemporalKind::Date => match NaiveDate::parse_from_str(iso, "%Y-%m-%d") {
            Ok(d) => query.bind(d),
            Err(_) => query.bind(iso.to_string()),
        },
        TemporalKind::Time => match NaiveTime::parse_from_str(iso, "%H:%M:%S%.f") {
            Ok(t) => query.bind(t),
            Err(_) => query.bind(iso.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names_bucket_to_logical() {
        assert_eq!(logical_for("BOOL"), LogicalType::Boolean);
        assert_eq!(logical_for("INT2"), LogicalType::Integer);
        assert_eq!(logical_for("INT4"), LogicalType::Integer);
        assert_eq!(logical_for("INT8"), LogicalType::Integer);
        assert_eq!(logical_for("FLOAT4"), LogicalType::Float);
        assert_eq!(logical_for("FLOAT8"), LogicalType::Float);
        assert_eq!(logical_for("NUMERIC"), LogicalType::Decimal);
        assert_eq!(logical_for("MONEY"), LogicalType::Decimal);
        assert_eq!(logical_for("UUID"), LogicalType::Uuid);
        assert_eq!(logical_for("TIMESTAMPTZ"), LogicalType::DateTime);
        assert_eq!(logical_for("TIMESTAMP"), LogicalType::DateTime);
        assert_eq!(logical_for("DATE"), LogicalType::Date);
        assert_eq!(logical_for("TIME"), LogicalType::Time);
        assert_eq!(logical_for("JSON"), LogicalType::Json);
        assert_eq!(logical_for("JSONB"), LogicalType::Json);
        assert_eq!(logical_for("BYTEA"), LogicalType::Binary);
        assert_eq!(logical_for("TEXT"), LogicalType::Text);
        assert_eq!(logical_for("VARCHAR"), LogicalType::Text);
        assert_eq!(logical_for("BPCHAR"), LogicalType::Text);
        assert_eq!(logical_for("NAME"), LogicalType::Text);
        // Arrays / enums / unknowns fall through to Other.
        assert_eq!(logical_for("_INT4"), LogicalType::Other);
        assert_eq!(logical_for("TEXT[]"), LogicalType::Other);
        assert_eq!(logical_for("my_enum"), LogicalType::Other);
    }

    #[test]
    fn unsupported_lowercases_type_and_keeps_empty_text() {
        match unsupported("_INT4") {
            CellValue::Unsupported { type_name, text } => {
                assert_eq!(type_name, "_int4");
                assert!(text.is_empty());
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
