//! Conversions between MySQL (sqlx) values and Selene's driver-neutral
//! [`CellValue`] / [`Column`] vocabulary.
//!
//! ## MySQL is statically typed
//! Like Postgres, every MySQL column has a fixed type known from the result
//! metadata, so both paths key off the column's type **name**. sqlx reports the
//! conventional uppercase MySQL name via `type_info().name()`, e.g. `INT`,
//! `BIGINT`, `DECIMAL`, `DATETIME`, `VARCHAR`, `JSON`. The value path decodes
//! each cell as `Option<T>` for the mapped Rust type `T`:
//! - a `None` (SQL `NULL`) becomes [`CellValue::Null`];
//! - a decode failure degrades to [`CellValue::Unsupported`] carrying the type
//!   name (never a panic, never aborting the result set).
//!
//! ## MySQL specifics vs Postgres
//! - **No native boolean**: `TINYINT(1)` is MySQL's boolean and sqlx reports it
//!   as the name `BOOLEAN`; wider `TINYINT(M)` is reported as `TINYINT`. We map
//!   `BOOLEAN` to [`CellValue::Bool`] and the integer widths to [`CellValue::I64`].
//! - **Unsigned integers**: sqlx reports unsigned columns with distinct names
//!   (`INT UNSIGNED`, `BIGINT UNSIGNED`, …). Every width up to `INT UNSIGNED`
//!   fits in `i64`; only `BIGINT UNSIGNED` can exceed `i64::MAX`, so it decodes
//!   `i64` first and falls back to a `u64`-rendered [`CellValue::Decimal`]
//!   (lossless) for the out-of-range tail.
//! - **No native UUID type**: UUIDs live in `CHAR(36)`/`BINARY(16)` columns and
//!   surface as `String`/`Bytes` accordingly.
//! - **`TIMESTAMP`** is returned by MySQL without an offset, so it maps to a
//!   naive [`TemporalKind::DateTime`] (not `DateTimeOffset`).
//! - **`TIME`** can exceed 24h or be negative (it is an interval, not a clock
//!   time); if a `NaiveTime` decode fails, the cell falls back to a `String` so
//!   the value is still shown rather than flagged Unsupported.

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::mysql::{MySqlArguments, MySqlRow};
use sqlx::{Column as _, Row as _, TypeInfo as _};

use crate::driver::shared::convert::{decimal_to_string, iso_date, iso_naive_dt, iso_time};
use crate::value::{CellValue, Column, LogicalType, TemporalKind};

/// Build Selene column metadata from a MySQL result row's columns.
///
/// `db_type` is the lowercased conventional MySQL type name; `logical` is
/// bucketed from it. JSON columns are tagged [`LogicalType::Json`]. Result
/// columns carry no nullability flag here, so `nullable` is `None` (the
/// introspection path fills that in).
pub(crate) fn columns_of(row: &MySqlRow) -> Vec<Column> {
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

/// Convert one MySQL row into a vector of neutral cell values, one per column.
pub(crate) fn convert_row(row: &MySqlRow) -> Vec<CellValue> {
    (0..row.len()).map(|i| cell_at(row, i)).collect()
}

/// Convert the cell at column index `i`, dispatching on the column's MySQL type
/// name (uppercase canonical form from sqlx, e.g. `"BIGINT"`, `"DATETIME"`).
fn cell_at(row: &MySqlRow, i: usize) -> CellValue {
    let type_name = match row.columns().get(i) {
        Some(c) => c.type_info().name(),
        // Index is in-bounds (0..row.len()); a missing column is unexpected.
        None => return CellValue::Null,
    };

    match type_name {
        // `TINYINT(1)` — MySQL's conventional boolean. Decoded as i8 (0/1) so the
        // wire protocol's TINYINT representation is read, then narrowed to Bool.
        "BOOLEAN" => get_map(row, i, type_name, |v: i8| CellValue::Bool(v != 0)),

        // Signed/unsigned integer widths that all fit in i64. `INT UNSIGNED`'s
        // max (~4.29e9) and every narrower unsigned width fit, so they decode as
        // i64 directly. YEAR is a 4-digit year, also an i64.
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "YEAR" | "TINYINT UNSIGNED"
        | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED" => {
            get_map(row, i, type_name, |v: i64| CellValue::I64(v))
        }

        // BIGINT: signed fits in i64; BIGINT UNSIGNED can exceed i64::MAX, so on a
        // failed i64 decode we read it as u64 and render it as a lossless decimal
        // string rather than truncate.
        "BIGINT" | "BIGINT UNSIGNED" => bigint_cell(row, i, type_name),

        "FLOAT" => get_map(row, i, type_name, |v: f32| CellValue::F64(v as f64)),
        "DOUBLE" => get_map(row, i, type_name, |v: f64| CellValue::F64(v)),

        // Exact numeric: decode to rust_decimal and render losslessly, degrading
        // to a string decode and finally Unsupported.
        "DECIMAL" => decimal_cell(row, i, type_name),

        // DATETIME decodes natively as a naive datetime.
        "DATETIME" => get_map(row, i, type_name, |v: NaiveDateTime| CellValue::DateTime {
            iso: iso_naive_dt(&v),
            kind: TemporalKind::DateTime,
        }),
        // TIMESTAMP: sqlx only decodes it as `DateTime<Utc>` (a TIMESTAMP is
        // stored in UTC and converted to the session time zone). Selene treats it
        // as a wall-clock `DateTime` (not a DateTimeOffset — the value carries no
        // offset the user chose), so we render the naive UTC datetime. With the
        // session at UTC this is exactly the stored wall-clock value.
        "TIMESTAMP" => timestamp_cell(row, i, type_name),
        "DATE" => get_map(row, i, type_name, |v: NaiveDate| CellValue::DateTime {
            iso: iso_date(&v),
            kind: TemporalKind::Date,
        }),
        // TIME is an interval (can be > 24h or negative) so a NaiveTime decode may
        // fail; fall back to the textual form rather than dropping the value.
        "TIME" => time_cell(row, i, type_name),

        // JSON: decode the document and render its canonical text.
        "JSON" => get_map(row, i, type_name, |v: serde_json::Value| {
            CellValue::String(v.to_string())
        }),

        // Binary families. BIT decodes to a byte vec under sqlx.
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY" | "BIT" => {
            get_map(row, i, type_name, |v: Vec<u8>| CellValue::Bytes(v))
        }

        // Character families (incl. ENUM/SET, which decode as their string label).
        // MySQL has no native UUID type — a UUID stored in CHAR(36) surfaces here
        // as a String.
        "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => {
            get_map(row, i, type_name, CellValue::String)
        }

        // Anything else (GEOMETRY, future types) — preserve the type name losslessly.
        _ => unsupported(type_name),
    }
}

/// Decode column `i` as `Option<T>` and map the present value through `f`.
///
/// `None` (SQL NULL) → [`CellValue::Null`]; a decode error → [`CellValue::Unsupported`]
/// carrying the type name (so one odd cell never aborts the set).
fn get_map<T>(
    row: &MySqlRow,
    i: usize,
    type_name: &str,
    f: impl FnOnce(T) -> CellValue,
) -> CellValue
where
    T: for<'r> sqlx::Decode<'r, sqlx::MySql> + sqlx::Type<sqlx::MySql>,
{
    match row.try_get::<Option<T>, _>(i) {
        Ok(Some(v)) => f(v),
        Ok(None) => CellValue::Null,
        Err(_) => unsupported(type_name),
    }
}

/// Decode a BIGINT cell. A signed BIGINT decodes as `i64`; a `BIGINT UNSIGNED`
/// value above `i64::MAX` fails that decode, so we read it as `u64` and keep it
/// exact as a [`CellValue::Decimal`] string. A final failure flags Unsupported.
fn bigint_cell(row: &MySqlRow, i: usize, type_name: &str) -> CellValue {
    match row.try_get::<Option<i64>, _>(i) {
        Ok(Some(v)) => CellValue::I64(v),
        Ok(None) => CellValue::Null,
        Err(_) => match row.try_get::<Option<u64>, _>(i) {
            Ok(Some(u)) => CellValue::Decimal(u.to_string()),
            Ok(None) => CellValue::Null,
            Err(_) => unsupported(type_name),
        },
    }
}

/// Decode a `TIMESTAMP` cell. sqlx decodes MySQL `TIMESTAMP` only as
/// `DateTime<Utc>`, so we read that and emit the naive (offset-stripped) datetime
/// as a [`TemporalKind::DateTime`] — MySQL `TIMESTAMP` carries no user-chosen
/// offset, so it is a wall-clock value, not a `DateTimeOffset`.
fn timestamp_cell(row: &MySqlRow, i: usize, type_name: &str) -> CellValue {
    get_map(row, i, type_name, |v: DateTime<Utc>| CellValue::DateTime {
        iso: iso_naive_dt(&v.naive_utc()),
        kind: TemporalKind::DateTime,
    })
}

/// Decode a `DECIMAL` cell to a lossless decimal string, degrading to a string
/// decode and finally [`CellValue::Unsupported`] on failure.
fn decimal_cell(row: &MySqlRow, i: usize, type_name: &str) -> CellValue {
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

/// Decode a `TIME` cell as a clock time, falling back to its textual form when
/// the value is outside `NaiveTime`'s range (MySQL `TIME` is an interval and can
/// be negative or exceed 24h). A final failure flags Unsupported.
fn time_cell(row: &MySqlRow, i: usize, type_name: &str) -> CellValue {
    match row.try_get::<Option<NaiveTime>, _>(i) {
        Ok(Some(t)) => CellValue::DateTime {
            iso: iso_time(&t),
            kind: TemporalKind::Time,
        },
        Ok(None) => CellValue::Null,
        Err(_) => match row.try_get::<Option<String>, _>(i) {
            Ok(Some(s)) => CellValue::String(s),
            Ok(None) => CellValue::Null,
            Err(_) => unsupported(type_name),
        },
    }
}

/// An [`CellValue::Unsupported`] carrying the (lowercased) MySQL type name and
/// empty text — the type name is enough for the UI to flag the cell, and we keep
/// potentially large/binary payloads out of the value.
fn unsupported(type_name: &str) -> CellValue {
    CellValue::Unsupported {
        type_name: type_name.to_ascii_lowercase(),
        text: String::new(),
    }
}

/// Bucket a MySQL type name into Selene's coarse [`LogicalType`] for UI
/// alignment/formatting. Unknown names fall through to [`LogicalType::Other`].
fn logical_for(type_name: &str) -> LogicalType {
    match type_name {
        "BOOLEAN" => LogicalType::Boolean,
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" | "YEAR" | "TINYINT UNSIGNED"
        | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED" | "BIGINT UNSIGNED" => {
            LogicalType::Integer
        }
        "FLOAT" | "DOUBLE" => LogicalType::Float,
        "DECIMAL" => LogicalType::Decimal,
        "DATETIME" | "TIMESTAMP" => LogicalType::DateTime,
        "DATE" => LogicalType::Date,
        "TIME" => LogicalType::Time,
        "JSON" => LogicalType::Json,
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY" | "BIT" => {
            LogicalType::Binary
        }
        "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => {
            LogicalType::Text
        }
        _ => LogicalType::Other,
    }
}

/// Bind a neutral [`CellValue`] onto a MySQL query for the import write path,
/// mirroring the read path's vocabulary.
///
/// ## Why this binds *native* types
/// MySQL coerces more leniently than Postgres, but binding the native Rust type
/// is the safest and most consistent choice (it mirrors the Postgres driver):
/// decimals parse back to [`rust_decimal::Decimal`], temporal values parse back
/// into their `chrono` type, ints/floats/bytes bind directly. MySQL has no
/// native boolean, so a [`CellValue::Bool`] binds as an `i64` `0`/`1` (the
/// destination is a `TINYINT`). A UUID has no native MySQL type either, so it
/// binds as text (the destination is a `CHAR`/`VARCHAR`). If a parse fails — a
/// value the read path produced as, say, `Decimal` but which is not re-parseable
/// — we fall back to binding the text so MySQL surfaces a clear coercion error
/// rather than the driver silently dropping data.
///
/// `NULL` binds as a typed `None`.
pub(crate) fn bind_value<'q>(
    query: sqlx::query::Query<'q, sqlx::MySql, MySqlArguments>,
    value: &CellValue,
) -> sqlx::query::Query<'q, sqlx::MySql, MySqlArguments> {
    match value {
        CellValue::Null => query.bind(None::<String>),
        // No native bool in MySQL: bind 0/1 for the TINYINT destination.
        CellValue::Bool(b) => query.bind(if *b { 1_i64 } else { 0_i64 }),
        CellValue::I64(n) => query.bind(*n),
        CellValue::F64(f) => query.bind(*f),
        CellValue::Bytes(b) => query.bind(b.clone()),
        CellValue::String(s) => query.bind(s.clone()),
        // Exact numeric: parse into rust_decimal so MySQL gets a numeric value.
        // On a parse failure bind the text and let MySQL report the mismatch.
        CellValue::Decimal(s) => match rust_decimal::Decimal::from_str_exact(s) {
            Ok(d) => query.bind(d),
            Err(_) => query.bind(s.clone()),
        },
        // MySQL has no native UUID type; bind the canonical text.
        CellValue::Uuid(s) => query.bind(s.clone()),
        CellValue::DateTime { iso, kind } => bind_datetime(query, iso, *kind),
        CellValue::Unsupported { text, .. } => query.bind(text.clone()),
    }
}

/// Bind a temporal [`CellValue::DateTime`] by parsing its ISO text into the
/// chrono type matching `kind` and binding that native value. MySQL has no
/// timezone-aware column type, so a `DateTimeOffset` is parsed to a naive
/// datetime (its wall-clock part). Falls back to binding the text on a parse
/// failure.
fn bind_datetime<'q>(
    query: sqlx::query::Query<'q, sqlx::MySql, MySqlArguments>,
    iso: &str,
    kind: TemporalKind,
) -> sqlx::query::Query<'q, sqlx::MySql, MySqlArguments> {
    match kind {
        // `%.f` matches the optional fractional second emitted by the read path.
        TemporalKind::DateTime | TemporalKind::DateTimeOffset => {
            // For an offset value, take the wall-clock prefix before the offset/
            // 'Z' (MySQL DATETIME/TIMESTAMP store no zone). Plain naive datetimes
            // pass through unchanged.
            let naive = iso.split(['+', 'Z']).next().unwrap_or(iso);
            match NaiveDateTime::parse_from_str(naive, "%Y-%m-%dT%H:%M:%S%.f") {
                Ok(dt) => query.bind(dt),
                Err(_) => query.bind(iso.to_string()),
            }
        }
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
        assert_eq!(logical_for("BOOLEAN"), LogicalType::Boolean);
        assert_eq!(logical_for("TINYINT"), LogicalType::Integer);
        assert_eq!(logical_for("SMALLINT"), LogicalType::Integer);
        assert_eq!(logical_for("MEDIUMINT"), LogicalType::Integer);
        assert_eq!(logical_for("INT"), LogicalType::Integer);
        assert_eq!(logical_for("BIGINT"), LogicalType::Integer);
        assert_eq!(logical_for("YEAR"), LogicalType::Integer);
        // Unsigned widths still bucket as integers.
        assert_eq!(logical_for("INT UNSIGNED"), LogicalType::Integer);
        assert_eq!(logical_for("BIGINT UNSIGNED"), LogicalType::Integer);
        assert_eq!(logical_for("FLOAT"), LogicalType::Float);
        assert_eq!(logical_for("DOUBLE"), LogicalType::Float);
        assert_eq!(logical_for("DECIMAL"), LogicalType::Decimal);
        assert_eq!(logical_for("DATETIME"), LogicalType::DateTime);
        assert_eq!(logical_for("TIMESTAMP"), LogicalType::DateTime);
        assert_eq!(logical_for("DATE"), LogicalType::Date);
        assert_eq!(logical_for("TIME"), LogicalType::Time);
        assert_eq!(logical_for("JSON"), LogicalType::Json);
        assert_eq!(logical_for("BLOB"), LogicalType::Binary);
        assert_eq!(logical_for("VARBINARY"), LogicalType::Binary);
        assert_eq!(logical_for("BIT"), LogicalType::Binary);
        assert_eq!(logical_for("CHAR"), LogicalType::Text);
        assert_eq!(logical_for("VARCHAR"), LogicalType::Text);
        assert_eq!(logical_for("TEXT"), LogicalType::Text);
        assert_eq!(logical_for("LONGTEXT"), LogicalType::Text);
        assert_eq!(logical_for("ENUM"), LogicalType::Text);
        assert_eq!(logical_for("SET"), LogicalType::Text);
        // Unknowns fall through to Other.
        assert_eq!(logical_for("GEOMETRY"), LogicalType::Other);
        assert_eq!(logical_for("POINT"), LogicalType::Other);
    }

    #[test]
    fn unsupported_lowercases_type_and_keeps_empty_text() {
        match unsupported("GEOMETRY") {
            CellValue::Unsupported { type_name, text } => {
                assert_eq!(type_name, "geometry");
                assert!(text.is_empty());
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
