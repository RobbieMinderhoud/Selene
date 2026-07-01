//! Conversions from BSON values to Selene's driver-neutral [`CellValue`] /
//! [`LogicalType`] vocabulary.
//!
//! MongoDB is schemaless: documents in a collection need share no fields and a
//! given field can hold different BSON types across documents. So unlike the SQL
//! drivers there is no server-declared column list — the streaming layer derives
//! columns by sampling documents (see [`super::stream`]) and this module supplies
//! the per-value type bucketing it uses ([`bson_type_name`] / [`logical_for`]).
//!
//! Design points:
//! - **Decimal128 is kept as a string** (like the SQL drivers' `DECIMAL`) so
//!   exact numerics never round-trip through `f64`.
//! - **Nested documents/arrays are carried as their *relaxed* extended-JSON
//!   text** ([`CellValue::Document`] / [`CellValue::Array`]). Relaxed extjson
//!   renders `{"k":1}` rather than the canonical `{"k":{"$numberInt":"1"}}`, so
//!   the grid shows values a human recognises.
//! - **ObjectId → its 24-char hex string**; **Binary → raw bytes**; temporal →
//!   an RFC-3339 UTC string. Exotic BSON types (Timestamp, RegularExpression,
//!   JavaScript, Symbol, DbPointer, Min/MaxKey) degrade to a readable relaxed-
//!   extjson string rather than being dropped — lossless-enough for display.

use bson::Bson;

use crate::value::{CellValue, LogicalType, TemporalKind};

/// Convert a single BSON value into a neutral [`CellValue`].
pub(crate) fn bson_to_cell(b: &Bson) -> CellValue {
    match b {
        Bson::Double(d) => CellValue::F64(*d),
        // BSON has two integer widths; both fit i64 losslessly.
        Bson::Int32(i) => CellValue::I64(*i as i64),
        Bson::Int64(i) => CellValue::I64(*i),
        // Keep exact decimals as text (Decimal128's Display is its canonical
        // string form), mirroring the SQL drivers' lossless DECIMAL handling.
        Bson::Decimal128(d) => CellValue::Decimal(d.to_string()),
        Bson::Boolean(v) => CellValue::Bool(*v),
        Bson::String(s) => CellValue::String(s.clone()),
        // `null` and the legacy `undefined` both surface as SQL NULL.
        Bson::Null | Bson::Undefined => CellValue::Null,
        Bson::DateTime(dt) => CellValue::DateTime {
            // BSON DateTime is always UTC milliseconds-since-epoch; render it as
            // an RFC-3339 UTC instant. `try_to_rfc3339_string` needs no extra
            // bson feature (unlike `to_chrono`, gated behind `chrono-0_4`) and
            // emits the canonical `…Z` UTC form. A value outside the formattable
            // range degrades to `Unsupported` rather than panicking.
            iso: match dt.try_to_rfc3339_string() {
                Ok(iso) => iso,
                Err(_) => {
                    return CellValue::Unsupported {
                        type_name: "date".to_string(),
                        text: dt.timestamp_millis().to_string(),
                    }
                }
            },
            kind: TemporalKind::DateTime,
        },
        // ObjectId reads as its canonical 24-char hex string.
        Bson::ObjectId(oid) => CellValue::String(oid.to_hex()),
        Bson::Binary(bin) => CellValue::Bytes(bin.bytes.clone()),
        // Nested document / array → their relaxed extended-JSON text so the grid
        // shows human-readable JSON (`{"k":1}`), not canonical extjson.
        Bson::Document(_) | Bson::Array(_) => {
            let json = relaxed_extjson(b);
            if matches!(b, Bson::Array(_)) {
                CellValue::Array(json)
            } else {
                CellValue::Document(json)
            }
        }
        // Everything else (Timestamp, RegularExpression, JavaScript(WithScope),
        // Symbol, DbPointer, MinKey, MaxKey, and any future variant) has no
        // dedicated neutral cell. Rather than drop it, carry a readable relaxed-
        // extjson rendering as a plain string so the value survives losslessly.
        _ => CellValue::String(relaxed_extjson(b)),
    }
}

/// Render a BSON value as a **relaxed** extended-JSON string.
///
/// Relaxed extjson keeps ordinary numbers/strings/bools as plain JSON and only
/// uses `$`-prefixed wrappers for types JSON cannot express (dates, ObjectIds,
/// …). `into_relaxed_extjson` consumes the value, so we clone first — cheap for
/// the scalar fallbacks and acceptable for the nested-document display path.
fn relaxed_extjson(b: &Bson) -> String {
    let value = b.clone().into_relaxed_extjson();
    // `serde_json::to_string` of a `serde_json::Value` cannot fail.
    serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string())
}

/// The backend type name Selene reports for a BSON value, used for
/// [`crate::value::Column::db_type`]. Names follow MongoDB's own `$type`
/// aliases (e.g. `"objectId"`, `"double"`, `"date"`).
pub(crate) fn bson_type_name(b: &Bson) -> &'static str {
    match b {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(_) => "binData",
        Bson::Undefined => "undefined",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "date",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::JavaScriptCodeWithScope(_) => "javascriptWithScope",
        Bson::Symbol(_) => "symbol",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "timestamp",
        Bson::Decimal128(_) => "decimal",
        Bson::MinKey => "minKey",
        Bson::MaxKey => "maxKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

/// Bucket a BSON value into Selene's coarse [`LogicalType`] for UI alignment and
/// default formatting.
pub(crate) fn logical_for(b: &Bson) -> LogicalType {
    match b {
        // ObjectId is a 12-byte identifier we render as hex text — treat it as
        // text for alignment (left), like a string key.
        Bson::ObjectId(_) | Bson::String(_) => LogicalType::Text,
        Bson::Double(_) => LogicalType::Float,
        Bson::Int32(_) | Bson::Int64(_) => LogicalType::Integer,
        Bson::Decimal128(_) => LogicalType::Decimal,
        Bson::Boolean(_) => LogicalType::Boolean,
        Bson::DateTime(_) => LogicalType::DateTime,
        Bson::Document(_) | Bson::Array(_) => LogicalType::Json,
        Bson::Binary(_) => LogicalType::Binary,
        Bson::Null | Bson::Undefined => LogicalType::Null,
        _ => LogicalType::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::{doc, oid::ObjectId, spec::BinarySubtype, Binary, Bson};
    use std::str::FromStr;

    #[test]
    fn scalars_convert() {
        assert_eq!(bson_to_cell(&Bson::Double(1.5)), CellValue::F64(1.5));
        assert_eq!(bson_to_cell(&Bson::Int32(7)), CellValue::I64(7));
        assert_eq!(
            bson_to_cell(&Bson::Int64(9_000_000_000)),
            CellValue::I64(9_000_000_000)
        );
        assert_eq!(bson_to_cell(&Bson::Boolean(true)), CellValue::Bool(true));
        assert_eq!(
            bson_to_cell(&Bson::String("hi".into())),
            CellValue::String("hi".into())
        );
    }

    #[test]
    fn null_and_undefined_map_to_null() {
        assert_eq!(bson_to_cell(&Bson::Null), CellValue::Null);
        assert_eq!(bson_to_cell(&Bson::Undefined), CellValue::Null);
    }

    #[test]
    fn decimal128_is_lossless_string() {
        let d = bson::Decimal128::from_str("123.4567").expect("parse decimal128");
        assert_eq!(
            bson_to_cell(&Bson::Decimal128(d)),
            CellValue::Decimal("123.4567".into())
        );
    }

    #[test]
    fn objectid_is_hex_string() {
        let oid = ObjectId::from_str("507f1f77bcf86cd799439011").expect("parse oid");
        assert_eq!(
            bson_to_cell(&Bson::ObjectId(oid)),
            CellValue::String("507f1f77bcf86cd799439011".into())
        );
    }

    #[test]
    fn datetime_is_rfc3339_utc() {
        // 2021-01-01T00:00:00Z as ms since epoch.
        let dt = bson::DateTime::from_millis(1_609_459_200_000);
        match bson_to_cell(&Bson::DateTime(dt)) {
            CellValue::DateTime { iso, kind } => {
                assert_eq!(kind, TemporalKind::DateTime);
                assert!(iso.starts_with("2021-01-01T00:00:00"), "got {iso}");
                // RFC-3339 UTC canonical form ends in `Z`.
                assert!(iso.ends_with('Z'), "got {iso}");
            }
            other => panic!("expected DateTime, got {other:?}"),
        }
    }

    #[test]
    fn binary_is_bytes() {
        let bin = Binary {
            subtype: BinarySubtype::Generic,
            bytes: vec![1, 2, 3],
        };
        assert_eq!(
            bson_to_cell(&Bson::Binary(bin)),
            CellValue::Bytes(vec![1, 2, 3])
        );
    }

    #[test]
    fn nested_document_is_relaxed_extjson() {
        let d = Bson::Document(doc! { "k": 1, "s": "v" });
        match bson_to_cell(&d) {
            CellValue::Document(json) => {
                // Relaxed extjson keeps the int as a plain `1`, not `$numberInt`.
                assert!(json.contains("\"k\":1"), "got {json}");
                assert!(json.contains("\"s\":\"v\""), "got {json}");
                assert!(!json.contains("$number"), "should be relaxed: {json}");
            }
            other => panic!("expected Document, got {other:?}"),
        }
    }

    #[test]
    fn nested_array_is_relaxed_extjson() {
        let a = Bson::Array(vec![Bson::Int32(1), Bson::String("x".into())]);
        match bson_to_cell(&a) {
            CellValue::Array(json) => {
                assert_eq!(json, "[1,\"x\"]");
            }
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn exotic_type_degrades_to_readable_string() {
        // MaxKey/MinKey have no neutral cell; they render as a relaxed-extjson
        // string rather than being dropped.
        match bson_to_cell(&Bson::MaxKey) {
            CellValue::String(s) => assert!(s.contains("$maxKey"), "got {s}"),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn type_names_cover_the_common_arms() {
        assert_eq!(bson_type_name(&Bson::Double(0.0)), "double");
        assert_eq!(bson_type_name(&Bson::Int32(0)), "int");
        assert_eq!(bson_type_name(&Bson::Int64(0)), "long");
        assert_eq!(bson_type_name(&Bson::String(String::new())), "string");
        assert_eq!(bson_type_name(&Bson::Boolean(true)), "bool");
        assert_eq!(bson_type_name(&Bson::Null), "null");
        assert_eq!(bson_type_name(&Bson::Document(doc! {})), "object");
        assert_eq!(bson_type_name(&Bson::Array(vec![])), "array");
        assert_eq!(bson_type_name(&Bson::ObjectId(ObjectId::new())), "objectId");
        assert_eq!(
            bson_type_name(&Bson::DateTime(bson::DateTime::from_millis(0))),
            "date"
        );
    }

    #[test]
    fn logical_buckets_match_expectations() {
        assert_eq!(
            logical_for(&Bson::ObjectId(ObjectId::new())),
            LogicalType::Text
        );
        assert_eq!(logical_for(&Bson::String(String::new())), LogicalType::Text);
        assert_eq!(logical_for(&Bson::Double(0.0)), LogicalType::Float);
        assert_eq!(logical_for(&Bson::Int32(0)), LogicalType::Integer);
        assert_eq!(logical_for(&Bson::Int64(0)), LogicalType::Integer);
        assert_eq!(logical_for(&Bson::Boolean(true)), LogicalType::Boolean);
        assert_eq!(
            logical_for(&Bson::DateTime(bson::DateTime::from_millis(0))),
            LogicalType::DateTime
        );
        assert_eq!(logical_for(&Bson::Document(doc! {})), LogicalType::Json);
        assert_eq!(logical_for(&Bson::Array(vec![])), LogicalType::Json);
        assert_eq!(logical_for(&Bson::Null), LogicalType::Null);
        assert_eq!(logical_for(&Bson::MaxKey), LogicalType::Other);
    }
}
