//! Helpers shared by every sqlx-backed driver (SQLite today; Postgres/MySQL
//! later).
//!
//! sqlx exposes one uniform surface across its backends, so the genuinely
//! backend-agnostic plumbing — streaming a `fetch_many` result into a
//! [`RowSink`](crate::driver::RowSink), capping bound parameters per `INSERT`,
//! and a couple of value formatters — lives here once instead of being copied
//! into each driver. Each driver supplies only the small backend-specific
//! closures (how to read a row's columns/cells and a query result's affected
//! count).
//!
//! Everything is `pub(crate)`: this is internal driver scaffolding, not part of
//! `selene-core`'s public API.
//!
//! > A shared value-formatting module (ISO datetimes, lossless decimals) is
//! > intentionally **not** here yet: SQLite stores temporal/decimal data as
//! > TEXT (no native storage class), so it formats nothing through such helpers.
//! > It will be added when Postgres/MySQL — which decode native `NaiveDateTime`/
//! > `Decimal` values — need it, keeping this module free of dead code today.

pub(crate) mod pump;
pub(crate) mod sub_batch;
