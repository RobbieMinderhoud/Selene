//! Conversions between SQLite (sqlx) values and Selene's neutral
//! [`CellValue`] / [`Column`] vocabulary.
//!
//! ## SQLite is dynamically typed
//! A SQLite value belongs to one of five **storage classes** (NULL, INTEGER,
//! REAL, TEXT, BLOB) *regardless* of the column's declared type — a column
//! declared `INTEGER` can hold a string. So the **value** path inspects each
//! cell's runtime storage class (via the value's `type_info().name()`) and maps
//! that, never trusting the declared type. The **column-metadata** path, in
//! contrast, reports the *declared* type (what `CREATE TABLE` said) for
//! `db_type` and buckets it into a [`LogicalType`] for the UI — the best
//! a-priori hint available before any row arrives.

use sqlx::sqlite::SqliteRow;
use sqlx::{Column as _, Row as _, TypeInfo as _, ValueRef as _};

use crate::value::{CellValue, Column, LogicalType};

/// Build Selene column metadata from a SQLite result row's columns.
///
/// `db_type` is the **declared** type name (`column.type_info().name()`), and
/// `logical` is bucketed from it. Result columns carry no nullability, so
/// `nullable` is `None` (the introspection path fills that in from
/// `PRAGMA table_info`).
pub(crate) fn columns_of(row: &SqliteRow) -> Vec<Column> {
    row.columns()
        .iter()
        .map(|c| {
            let declared = c.type_info().name();
            Column {
                name: c.name().to_string(),
                ordinal: c.ordinal(),
                db_type: declared.to_string(),
                logical: logical_for_declared(declared),
                nullable: None,
            }
        })
        .collect()
}

/// Convert one SQLite row into a vector of neutral cell values, one per column.
///
/// Each cell is mapped by its **runtime storage class** (NULL/INTEGER/REAL/
/// TEXT/BLOB), inspected through the value reference's type-info name, then
/// decoded with the matching typed `try_get`. A decode failure degrades to
/// [`CellValue::Unsupported`] (carrying the storage-class name) rather than
/// aborting the whole result set.
pub(crate) fn convert_row(row: &SqliteRow) -> Vec<CellValue> {
    (0..row.len()).map(|i| cell_at(row, i)).collect()
}

/// Convert the cell at column index `i`.
fn cell_at(row: &SqliteRow, i: usize) -> CellValue {
    // `try_get_raw` gives us the value reference, from which the storage class is
    // read. A failure here is unexpected (index is in-bounds) — treat as Null.
    let raw = match row.try_get_raw(i) {
        Ok(r) => r,
        Err(_) => return CellValue::Null,
    };
    if raw.is_null() {
        return CellValue::Null;
    }

    // The value's type-info name is the *storage class* for an actual value
    // ("INTEGER", "REAL", "TEXT", "BLOB"). Match on it and decode accordingly.
    let class = raw.type_info().name().to_ascii_uppercase();
    match class.as_str() {
        "INTEGER" => match row.try_get::<i64, _>(i) {
            Ok(v) => CellValue::I64(v),
            Err(_) => unsupported(&class),
        },
        "REAL" => match row.try_get::<f64, _>(i) {
            Ok(v) => CellValue::F64(v),
            Err(_) => unsupported(&class),
        },
        "TEXT" => match row.try_get::<String, _>(i) {
            Ok(v) => CellValue::String(v),
            Err(_) => unsupported(&class),
        },
        "BLOB" => match row.try_get::<Vec<u8>, _>(i) {
            Ok(v) => CellValue::Bytes(v),
            Err(_) => unsupported(&class),
        },
        // SQLite only has the five storage classes; anything else is unexpected.
        // Preserve it losslessly as text rather than dropping the cell.
        _ => match row.try_get::<String, _>(i) {
            Ok(v) => CellValue::Unsupported {
                type_name: class,
                text: v,
            },
            Err(_) => unsupported(&class),
        },
    }
}

fn unsupported(type_name: &str) -> CellValue {
    CellValue::Unsupported {
        type_name: type_name.to_string(),
        text: String::new(),
    }
}

/// Bucket a SQLite **declared** type name into Selene's coarse [`LogicalType`].
///
/// SQLite's type affinity rules are name-substring based; we apply the same
/// spirit here for the UI hint. The match is on the leading keyword so
/// parameterised forms (`VARCHAR(50)`, `DECIMAL(18,4)`) still bucket correctly.
fn logical_for_declared(declared: &str) -> LogicalType {
    let upper = declared.trim().to_ascii_uppercase();
    // Strip any `(...)` size/precision suffix and take the first word.
    let head = upper.split(['(', ' ']).next().unwrap_or("").trim();
    match head {
        "" => LogicalType::Other,
        "INTEGER" | "INT" | "TINYINT" | "SMALLINT" | "MEDIUMINT" | "BIGINT" | "INT2" | "INT8" => {
            LogicalType::Integer
        }
        "REAL" | "DOUBLE" | "FLOAT" => LogicalType::Float,
        "NUMERIC" | "DECIMAL" => LogicalType::Decimal,
        "BOOLEAN" | "BOOL" => LogicalType::Boolean,
        "DATE" => LogicalType::Date,
        "TIME" => LogicalType::Time,
        "DATETIME" | "TIMESTAMP" => LogicalType::DateTime,
        "BLOB" | "BINARY" | "VARBINARY" => LogicalType::Binary,
        "TEXT" | "CHAR" | "VARCHAR" | "NCHAR" | "NVARCHAR" | "CLOB" => LogicalType::Text,
        // Affinity fallbacks (applied to the *full* declared name, mirroring
        // SQLite's own substring-based affinity rules): a name containing
        // CHAR/CLOB/TEXT is TEXT, then anything with INT is integer. Checking the
        // whole string catches multi-word types like "NATIVE CHARACTER" and
        // "UNSIGNED BIG INT".
        _ if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") => {
            LogicalType::Text
        }
        _ if upper.contains("INT") => LogicalType::Integer,
        _ => LogicalType::Other,
    }
}

/// Bind a neutral [`CellValue`] onto a SQLite-bound query, mirroring
/// `columns_of`/`convert_row`'s read path.
///
/// Native scalars (int/float/bool/blob) bind as their SQLite type; decimals,
/// strings, UUIDs, datetimes, and any `Unsupported` text bind as TEXT — SQLite's
/// dynamic typing + column affinity coerces them on insert (ISO-8601 datetimes
/// and `.`-decimals are culture-independent), which preserves the exact text
/// without re-parsing. A `NULL` binds as a typed `Option::None`.
pub(crate) fn bind_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    value: &CellValue,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    match value {
        CellValue::Null => query.bind(None::<String>),
        CellValue::Bool(b) => query.bind(*b),
        CellValue::I64(n) => query.bind(*n),
        CellValue::F64(f) => query.bind(*f),
        CellValue::Bytes(b) => query.bind(b.clone()),
        CellValue::Decimal(s) | CellValue::String(s) | CellValue::Uuid(s) => query.bind(s.clone()),
        CellValue::DateTime { iso, .. } => query.bind(iso.clone()),
        CellValue::Unsupported { text, .. } => query.bind(text.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_type_buckets_to_logical() {
        assert_eq!(logical_for_declared("INTEGER"), LogicalType::Integer);
        assert_eq!(logical_for_declared("int"), LogicalType::Integer);
        assert_eq!(logical_for_declared("BIGINT"), LogicalType::Integer);
        assert_eq!(logical_for_declared("REAL"), LogicalType::Float);
        assert_eq!(logical_for_declared("DOUBLE"), LogicalType::Float);
        assert_eq!(logical_for_declared("TEXT"), LogicalType::Text);
        assert_eq!(logical_for_declared("VARCHAR(50)"), LogicalType::Text);
        assert_eq!(logical_for_declared("NVARCHAR(255)"), LogicalType::Text);
        assert_eq!(logical_for_declared("BLOB"), LogicalType::Binary);
        assert_eq!(logical_for_declared("NUMERIC"), LogicalType::Decimal);
        assert_eq!(logical_for_declared("DECIMAL(18,4)"), LogicalType::Decimal);
        assert_eq!(logical_for_declared("BOOLEAN"), LogicalType::Boolean);
        assert_eq!(logical_for_declared("DATE"), LogicalType::Date);
        assert_eq!(logical_for_declared("DATETIME"), LogicalType::DateTime);
        assert_eq!(logical_for_declared("TIMESTAMP"), LogicalType::DateTime);
        // Affinity fallbacks.
        assert_eq!(logical_for_declared("NATIVE CHARACTER"), LogicalType::Text);
        assert_eq!(
            logical_for_declared("UNSIGNED BIG INT"),
            LogicalType::Integer
        );
        // Unknown.
        assert_eq!(logical_for_declared("GEOMETRY"), LogicalType::Other);
        assert_eq!(logical_for_declared(""), LogicalType::Other);
    }
}
