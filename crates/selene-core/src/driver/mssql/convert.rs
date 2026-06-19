//! Conversions from tiberius (TDS) types to Selene's driver-neutral
//! [`CellValue`] / [`Column`] vocabulary.
//!
//! Two design points worth noting:
//!
//! * **Exact numerics never lose precision.** `DECIMAL`/`NUMERIC` arrive as a
//!   [`tiberius::numeric::Numeric`] backed by an `i128` plus a scale. We render
//!   that to a decimal string by hand (see [`numeric_to_string`]) rather than
//!   going through `f64`, so financial values round-trip exactly.
//! * **Temporal values reuse tiberius' tested chrono mappings.** Rather than
//!   re-deriving the day/fragment arithmetic, we lean on the `FromSql`
//!   conversions tiberius provides under its `chrono` feature, then format the
//!   resulting chrono value as ISO-8601 / RFC-3339.

use std::borrow::Cow;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use tiberius::numeric::Numeric;
use tiberius::xml::XmlData;
use tiberius::{Column as TiberiusColumn, ColumnData, ColumnType, FromSql, ToSql};

use crate::value::{CellValue, Column, LogicalType, TemporalKind};

/// Render a tiberius [`Numeric`] to an exact decimal string with no precision
/// loss, preserving sign and scale (e.g. `-12.3400`, `0.05`, `42`).
///
/// `Numeric` stores a signed `i128` mantissa (`value`) and a `scale` (number of
/// fractional digits). We format the absolute mantissa zero-padded to at least
/// `scale + 1` digits, then insert the decimal point `scale` places from the
/// right. This keeps trailing zeros that the column's declared scale implies.
fn numeric_to_string(n: Numeric) -> String {
    let scale = n.scale() as usize;
    let value = n.value();

    if scale == 0 {
        return value.to_string();
    }

    let negative = value < 0;
    // `unsigned_abs` yields a u128, sidestepping the i128::MIN overflow that
    // `value.abs()` would hit.
    let digits = value.unsigned_abs().to_string();

    // Need at least `scale + 1` digits so there is always an integer part
    // (possibly "0") to the left of the decimal point.
    let padded = if digits.len() <= scale {
        format!("{:0>width$}", digits, width = scale + 1)
    } else {
        digits
    };

    let split = padded.len() - scale;
    let (int_part, frac_part) = padded.split_at(split);

    let sign = if negative { "-" } else { "" };
    format!("{sign}{int_part}.{frac_part}")
}

/// Convert a single TDS cell into a [`CellValue`].
///
/// Every variant maps onto the closest neutral type; anything Selene does not
/// model (currently only XML and any future TDS additions) is preserved
/// losslessly as [`CellValue::Unsupported`] text so the pipeline never fails on
/// an exotic type.
pub fn cell_to_value(data: &ColumnData<'static>) -> CellValue {
    match data {
        ColumnData::U8(v) => opt(v, |&x| CellValue::I64(x as i64)),
        ColumnData::I16(v) => opt(v, |&x| CellValue::I64(x as i64)),
        ColumnData::I32(v) => opt(v, |&x| CellValue::I64(x as i64)),
        ColumnData::I64(v) => opt(v, |&x| CellValue::I64(x)),
        ColumnData::F32(v) => opt(v, |&x| CellValue::F64(x as f64)),
        ColumnData::F64(v) => opt(v, |&x| CellValue::F64(x)),
        ColumnData::Bit(v) => opt(v, |&x| CellValue::Bool(x)),
        ColumnData::String(v) => opt(v, |s| CellValue::String(s.to_string())),
        ColumnData::Guid(v) => opt(v, |u| CellValue::Uuid(u.to_string())),
        ColumnData::Binary(v) => opt(v, |b| CellValue::Bytes(b.to_vec())),
        ColumnData::Numeric(v) => opt(v, |&n| CellValue::Decimal(numeric_to_string(n))),

        // Temporal types: delegate to tiberius' chrono `FromSql` mappings, then
        // format to ISO-8601 / RFC-3339. NULLs surface as `Null`; a failed
        // conversion (should not happen for well-formed rows) degrades to
        // `Unsupported` rather than panicking.
        ColumnData::Date(_) => temporal::<NaiveDate>(data, TemporalKind::Date, "date", |d| {
            d.format("%Y-%m-%d").to_string()
        }),
        ColumnData::Time(_) => temporal::<NaiveTime>(data, TemporalKind::Time, "time", |t| {
            // %.f emits a fractional-second component only when non-zero.
            t.format("%H:%M:%S%.f").to_string()
        }),
        ColumnData::DateTime(_) => {
            temporal::<NaiveDateTime>(data, TemporalKind::DateTime, "datetime", iso_naive_dt)
        }
        ColumnData::SmallDateTime(_) => {
            temporal::<NaiveDateTime>(data, TemporalKind::DateTime, "smalldatetime", iso_naive_dt)
        }
        ColumnData::DateTime2(_) => {
            temporal::<NaiveDateTime>(data, TemporalKind::DateTime, "datetime2", iso_naive_dt)
        }
        ColumnData::DateTimeOffset(_) => temporal::<DateTime<FixedOffset>>(
            data,
            TemporalKind::DateTimeOffset,
            "datetimeoffset",
            |dt| dt.to_rfc3339(),
        ),

        // XML is not modelled as a first-class neutral type yet; keep its text.
        ColumnData::Xml(v) => match v {
            None => CellValue::Null,
            Some(xml) => CellValue::Unsupported {
                type_name: "xml".to_string(),
                text: xml_to_string(xml),
            },
        },
    }
}

/// A bound `INSERT` parameter built from a [`CellValue`] (the write path,
/// mirroring [`cell_to_value`]'s read path).
///
/// We bind native TDS types for bool/int/float/binary, and **text** for
/// decimals, datetimes, UUIDs and strings: SQL Server converts those exactly on
/// insert (ISO-8601 datetimes and `.`-decimals are culture-independent), which
/// avoids re-parsing into tiberius' internal `Numeric`/temporal representations.
/// A `NULL` binds as an untyped `nvarchar` null, which inserts as `NULL` into a
/// (nullable) column of any type.
pub enum SqlParam {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Bytes(Vec<u8>),
    Text(String),
}

impl ToSql for SqlParam {
    fn to_sql(&self) -> ColumnData<'_> {
        match self {
            SqlParam::Null => ColumnData::String(None),
            SqlParam::Bool(b) => ColumnData::Bit(Some(*b)),
            SqlParam::I64(n) => ColumnData::I64(Some(*n)),
            SqlParam::F64(f) => ColumnData::F64(Some(*f)),
            SqlParam::Bytes(b) => ColumnData::Binary(Some(Cow::Borrowed(b))),
            SqlParam::Text(s) => ColumnData::String(Some(Cow::Borrowed(s))),
        }
    }
}

/// Convert a neutral [`CellValue`] into a bound `INSERT` parameter.
pub fn value_to_param(value: &CellValue) -> SqlParam {
    match value {
        CellValue::Null => SqlParam::Null,
        CellValue::Bool(b) => SqlParam::Bool(*b),
        CellValue::I64(n) => SqlParam::I64(*n),
        CellValue::F64(f) => SqlParam::F64(*f),
        CellValue::Bytes(b) => SqlParam::Bytes(b.clone()),
        // All carry exact text that SQL Server converts to the destination type.
        CellValue::Decimal(s) | CellValue::String(s) | CellValue::Uuid(s) => {
            SqlParam::Text(s.clone())
        }
        CellValue::DateTime { iso, .. } => SqlParam::Text(iso.clone()),
        CellValue::Unsupported { text, .. } => SqlParam::Text(text.clone()),
    }
}

/// Map an `Option`-wrapped TDS value: `None` becomes [`CellValue::Null`],
/// `Some(x)` is passed through `f`.
#[inline]
fn opt<T>(value: &Option<T>, f: impl FnOnce(&T) -> CellValue) -> CellValue {
    match value {
        None => CellValue::Null,
        Some(v) => f(v),
    }
}

/// ISO-8601 for a naive datetime, with a fractional-second part only when
/// non-zero (`%.f`). The `T` separator keeps it ISO-compliant.
fn iso_naive_dt(dt: &NaiveDateTime) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%.f").to_string()
}

/// Render tiberius XML to its string form. `XmlData` exposes its content via
/// `as_ref()` / `Display`.
fn xml_to_string(xml: &XmlData) -> String {
    xml.to_string()
}

/// Convert a temporal cell by delegating to a tiberius `FromSql` chrono target
/// `T`, then formatting it. `None` (SQL NULL) yields [`CellValue::Null`]; a
/// conversion error degrades to [`CellValue::Unsupported`] carrying the type
/// name, so a single odd row can never abort a result set.
fn temporal<'a, T>(
    data: &'a ColumnData<'static>,
    kind: TemporalKind,
    type_name: &str,
    fmt: impl FnOnce(&T) -> String,
) -> CellValue
where
    T: FromSql<'a>,
{
    match T::from_sql(data) {
        Ok(Some(v)) => CellValue::DateTime { iso: fmt(&v), kind },
        Ok(None) => CellValue::Null,
        Err(_) => CellValue::Unsupported {
            type_name: type_name.to_string(),
            // We deliberately avoid embedding the raw Debug of the cell here to
            // keep potentially large/binary payloads out of the value; the type
            // name is enough for the UI to flag it.
            text: String::new(),
        },
    }
}

/// Convert tiberius result-set column metadata to Selene's [`Column`].
///
/// Result-set columns from TDS carry no nullability flag, so `nullable` is
/// `None`; the introspection path fills that in from `INFORMATION_SCHEMA`.
pub fn column_to_meta(col: &TiberiusColumn, ordinal: usize) -> Column {
    let ty = col.column_type();
    Column {
        name: col.name().to_string(),
        ordinal,
        db_type: db_type_name(ty).to_string(),
        logical: logical_type(ty),
        nullable: None,
    }
}

/// The backend type name Selene reports for a tiberius [`ColumnType`].
///
/// These are the conventional SQL Server type names. Some `ColumnType`s are
/// width-erased (e.g. `Intn`, `Floatn`); we report the family name in those
/// cases, which is the best the result-set metadata allows.
fn db_type_name(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Intn => "int",
        ColumnType::Float4 => "real",
        ColumnType::Float8 => "float",
        ColumnType::Floatn => "float",
        ColumnType::Money => "money",
        ColumnType::Money4 => "smallmoney",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Datetime => "datetime",
        ColumnType::Datetimen => "datetime",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigBinary => "binary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::Image => "image",
        ColumnType::NText => "ntext",
        ColumnType::SSVariant => "sql_variant",
    }
}

/// Bucket a tiberius [`ColumnType`] into Selene's coarse [`LogicalType`], used
/// by the UI for alignment and default formatting.
fn logical_type(ty: ColumnType) -> LogicalType {
    match ty {
        ColumnType::Null => LogicalType::Null,
        ColumnType::Bit | ColumnType::Bitn => LogicalType::Boolean,
        ColumnType::Int1
        | ColumnType::Int2
        | ColumnType::Int4
        | ColumnType::Int8
        | ColumnType::Intn => LogicalType::Integer,
        ColumnType::Float4 | ColumnType::Float8 | ColumnType::Floatn => LogicalType::Float,
        // MONEY is an exact 4-decimal numeric in SQL Server; treat it as Decimal
        // so it aligns/formats with other exact numerics.
        ColumnType::Money | ColumnType::Money4 | ColumnType::Decimaln | ColumnType::Numericn => {
            LogicalType::Decimal
        }
        ColumnType::Guid => LogicalType::Uuid,
        ColumnType::Daten => LogicalType::Date,
        ColumnType::Timen => LogicalType::Time,
        ColumnType::Datetime4
        | ColumnType::Datetime
        | ColumnType::Datetimen
        | ColumnType::Datetime2
        | ColumnType::DatetimeOffsetn => LogicalType::DateTime,
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => LogicalType::Binary,
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::Text
        | ColumnType::NText => LogicalType::Text,
        ColumnType::Xml => LogicalType::Json,
        ColumnType::Udt | ColumnType::SSVariant => LogicalType::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use tiberius::numeric::Numeric;
    use uuid::Uuid;

    #[test]
    fn numeric_string_is_lossless() {
        // 1234567 with scale 4 => 123.4567
        assert_eq!(
            numeric_to_string(Numeric::new_with_scale(1_234_567, 4)),
            "123.4567"
        );
        // Negative preserves sign.
        assert_eq!(
            numeric_to_string(Numeric::new_with_scale(-1_234_567, 4)),
            "-123.4567"
        );
        // Value smaller than the scale gets a leading zero integer part.
        assert_eq!(numeric_to_string(Numeric::new_with_scale(5, 2)), "0.05");
        assert_eq!(numeric_to_string(Numeric::new_with_scale(-5, 2)), "-0.05");
        // Scale 0 => plain integer, no decimal point.
        assert_eq!(numeric_to_string(Numeric::new_with_scale(42, 0)), "42");
        // Trailing zeros implied by scale are preserved.
        assert_eq!(numeric_to_string(Numeric::new_with_scale(100, 2)), "1.00");
        // Zero.
        assert_eq!(numeric_to_string(Numeric::new_with_scale(0, 4)), "0.0000");
    }

    #[test]
    fn numeric_handles_large_i128() {
        // A 38-digit-ish value with scale 0 must round-trip as-is.
        let big = 123_456_789_012_345_678_901_234_567_890i128;
        assert_eq!(
            numeric_to_string(Numeric::new_with_scale(big, 0)),
            big.to_string()
        );
    }

    #[test]
    fn integer_widths_map_to_i64() {
        assert_eq!(cell_to_value(&ColumnData::U8(Some(7))), CellValue::I64(7));
        assert_eq!(
            cell_to_value(&ColumnData::I16(Some(-3))),
            CellValue::I64(-3)
        );
        assert_eq!(
            cell_to_value(&ColumnData::I32(Some(70000))),
            CellValue::I64(70000)
        );
        assert_eq!(
            cell_to_value(&ColumnData::I64(Some(9_000_000_000))),
            CellValue::I64(9_000_000_000)
        );
    }

    #[test]
    fn nulls_map_to_null() {
        assert_eq!(cell_to_value(&ColumnData::I32(None)), CellValue::Null);
        assert_eq!(cell_to_value(&ColumnData::Bit(None)), CellValue::Null);
        assert_eq!(cell_to_value(&ColumnData::String(None)), CellValue::Null);
        assert_eq!(cell_to_value(&ColumnData::Numeric(None)), CellValue::Null);
        assert_eq!(cell_to_value(&ColumnData::Date(None)), CellValue::Null);
    }

    #[test]
    fn scalar_values_convert() {
        assert_eq!(
            cell_to_value(&ColumnData::Bit(Some(true))),
            CellValue::Bool(true)
        );
        assert_eq!(
            cell_to_value(&ColumnData::F64(Some(1.5))),
            CellValue::F64(1.5)
        );
        assert_eq!(
            cell_to_value(&ColumnData::String(Some(Cow::Borrowed("hi")))),
            CellValue::String("hi".to_string())
        );
        assert_eq!(
            cell_to_value(&ColumnData::Binary(Some(Cow::Borrowed(&[1u8, 2, 3])))),
            CellValue::Bytes(vec![1, 2, 3])
        );
    }

    #[test]
    fn guid_is_canonical_string() {
        let u = Uuid::parse_str("936da01f-9abd-4d9d-80c7-02af85c822a8").unwrap();
        assert_eq!(
            cell_to_value(&ColumnData::Guid(Some(u))),
            CellValue::Uuid("936da01f-9abd-4d9d-80c7-02af85c822a8".to_string())
        );
    }

    #[test]
    fn decimal_cell_uses_lossless_string() {
        let n = Numeric::new_with_scale(-1_234_567, 4);
        assert_eq!(
            cell_to_value(&ColumnData::Numeric(Some(n))),
            CellValue::Decimal("-123.4567".to_string())
        );
    }

    #[test]
    fn value_to_param_binds_native_and_text() {
        // Native scalars keep their TDS type.
        assert!(matches!(
            value_to_param(&CellValue::Bool(true)).to_sql(),
            ColumnData::Bit(Some(true))
        ));
        assert!(matches!(
            value_to_param(&CellValue::I64(42)).to_sql(),
            ColumnData::I64(Some(42))
        ));
        assert!(matches!(
            value_to_param(&CellValue::F64(1.5)).to_sql(),
            ColumnData::F64(Some(_))
        ));
        // Decimal/DateTime/Uuid/String all bind as nvarchar text.
        for v in [
            CellValue::Decimal("12.34".into()),
            CellValue::Uuid("936da01f-9abd-4d9d-80c7-02af85c822a8".into()),
            CellValue::String("hi".into()),
            CellValue::DateTime {
                iso: "2026-06-19T00:00:00".into(),
                kind: TemporalKind::DateTime,
            },
        ] {
            assert!(matches!(
                value_to_param(&v).to_sql(),
                ColumnData::String(Some(_))
            ));
        }
        // NULL binds as an untyped nvarchar null.
        assert!(matches!(
            value_to_param(&CellValue::Null).to_sql(),
            ColumnData::String(None)
        ));
        // Bytes stay binary.
        assert!(matches!(
            value_to_param(&CellValue::Bytes(vec![1, 2])).to_sql(),
            ColumnData::Binary(Some(_))
        ));
    }

    #[test]
    fn db_type_and_logical_mapping() {
        assert_eq!(db_type_name(ColumnType::NVarchar), "nvarchar");
        assert_eq!(logical_type(ColumnType::NVarchar), LogicalType::Text);
        assert_eq!(db_type_name(ColumnType::Int4), "int");
        assert_eq!(logical_type(ColumnType::Int4), LogicalType::Integer);
        assert_eq!(logical_type(ColumnType::Money), LogicalType::Decimal);
        assert_eq!(logical_type(ColumnType::Guid), LogicalType::Uuid);
        assert_eq!(
            logical_type(ColumnType::DatetimeOffsetn),
            LogicalType::DateTime
        );
        assert_eq!(logical_type(ColumnType::Daten), LogicalType::Date);
        assert_eq!(logical_type(ColumnType::Xml), LogicalType::Json);
    }
}
