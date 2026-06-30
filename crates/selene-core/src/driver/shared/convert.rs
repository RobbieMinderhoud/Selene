//! Value formatters shared by the native-typed sqlx drivers (Postgres and MySQL).
//!
//! Two families of value need a single, culture-independent rendering so they
//! round-trip losslessly through Selene's neutral [`CellValue`](crate::value::CellValue):
//!
//! * **Temporal values** decode to `chrono` types; we render them to ISO-8601 /
//!   RFC-3339, mirroring the mssql driver's formats exactly (`iso_naive_dt`,
//!   `iso_date`, `iso_time`, and `.to_rfc3339()` for timezone-aware values) so
//!   the grid and exporters see one format regardless of backend.
//! * **Exact numerics** decode to [`rust_decimal::Decimal`]; we render them with
//!   `Decimal::to_string`, which prints the full significand and scale with no
//!   `f64` round-trip — financial values stay exact.
//!
//! SQLite stores both families as TEXT and never decodes them into these native
//! types, so it does not use this module; that is why it is feature-gated to the
//! backends that do (avoiding dead code in a SQLite-only build).

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
// `DateTime`/`TimeZone` back the Postgres-only `iso_offset_dt`, so they are
// imported only when that backend is compiled (avoids an unused-import warning in
// a MySQL-only build).
#[cfg(feature = "postgres")]
use chrono::{DateTime, TimeZone};

/// ISO-8601 for a naive datetime, with a fractional-second part only when
/// non-zero (`%.f`). The `T` separator keeps it ISO-compliant — identical to the
/// mssql driver's `iso_naive_dt`.
pub(crate) fn iso_naive_dt(dt: &NaiveDateTime) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%.f").to_string()
}

/// ISO-8601 calendar date (`%Y-%m-%d`).
pub(crate) fn iso_date(d: &NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// ISO-8601 time of day, with a fractional-second part only when non-zero
/// (`%H:%M:%S%.f`).
pub(crate) fn iso_time(t: &NaiveTime) -> String {
    t.format("%H:%M:%S%.f").to_string()
}

/// RFC-3339 for a timezone-aware datetime (e.g. Postgres `TIMESTAMPTZ`). Works
/// for any [`TimeZone`] (`Utc`, `FixedOffset`, …); the offset is preserved in the
/// rendered string.
///
/// Only Postgres has a timezone-aware column type (`TIMESTAMPTZ`); MySQL's
/// `TIMESTAMP`/`DATETIME` are returned without an offset and use [`iso_naive_dt`].
/// So this formatter is gated to the Postgres backend to avoid a dead-code
/// warning in a MySQL-only build.
#[cfg(feature = "postgres")]
pub(crate) fn iso_offset_dt<Tz: TimeZone>(dt: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    dt.to_rfc3339()
}

/// Render a [`rust_decimal::Decimal`] to its exact decimal string (no `f64`
/// round-trip, so the full precision/scale is preserved).
pub(crate) fn decimal_to_string(d: rust_decimal::Decimal) -> String {
    d.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use rust_decimal::Decimal;
    use std::str::FromStr as _;

    #[test]
    fn naive_datetime_is_iso_with_optional_fraction() {
        // No fractional second => no trailing `.000`.
        let dt = NaiveDate::from_ymd_opt(2026, 6, 30)
            .unwrap()
            .and_hms_opt(14, 5, 9)
            .unwrap();
        assert_eq!(iso_naive_dt(&dt), "2026-06-30T14:05:09");

        // A sub-second part is emitted when present.
        let dt2 = NaiveDate::from_ymd_opt(2026, 6, 30)
            .unwrap()
            .and_hms_micro_opt(14, 5, 9, 123_456)
            .unwrap();
        assert_eq!(iso_naive_dt(&dt2), "2026-06-30T14:05:09.123456");
    }

    #[test]
    fn date_and_time_formats() {
        let d = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        assert_eq!(iso_date(&d), "2026-01-02");

        let t = NaiveTime::from_hms_opt(8, 9, 10).unwrap();
        assert_eq!(iso_time(&t), "08:09:10");
        let t2 = NaiveTime::from_hms_milli_opt(8, 9, 10, 250).unwrap();
        assert_eq!(iso_time(&t2), "08:09:10.250");
    }

    // `iso_offset_dt` is Postgres-only, so its test is gated to that backend too.
    #[cfg(feature = "postgres")]
    #[test]
    fn offset_datetime_is_rfc3339() {
        use chrono::{FixedOffset, TimeZone, Utc};
        // UTC renders with a `+00:00` offset.
        let utc = Utc
            .with_ymd_and_hms(2026, 6, 30, 12, 0, 0)
            .single()
            .unwrap();
        assert_eq!(iso_offset_dt(&utc), "2026-06-30T12:00:00+00:00");

        // A non-zero fixed offset is preserved in the rendered string.
        let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
        let local = plus2
            .with_ymd_and_hms(2026, 6, 30, 12, 0, 0)
            .single()
            .unwrap();
        assert_eq!(iso_offset_dt(&local), "2026-06-30T12:00:00+02:00");
    }

    #[test]
    fn decimal_is_lossless() {
        // Trailing zeros implied by the scale are preserved.
        assert_eq!(
            decimal_to_string(Decimal::from_str("123.4500").unwrap()),
            "123.4500"
        );
        assert_eq!(
            decimal_to_string(Decimal::from_str("-0.05").unwrap()),
            "-0.05"
        );
        assert_eq!(decimal_to_string(Decimal::from_str("42").unwrap()), "42");
        // A high-precision value round-trips exactly (no f64 truncation).
        let big = "79228162514264337593543950335"; // i96 max, scale 0
        assert_eq!(decimal_to_string(Decimal::from_str(big).unwrap()), big);
    }
}
