//! Helpers shared by every sqlx-backed driver (SQLite, Postgres, and MySQL).
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
//! [`convert`] holds the value formatters (ISO-8601 datetimes, lossless decimal
//! strings) that the native-typed sqlx drivers need. SQLite stores temporal/
//! decimal data as TEXT (no native chrono/`Decimal` decode), so it does not use
//! them — Postgres and MySQL do, decoding `chrono`/`rust_decimal` values and
//! formatting them here, which keeps a single culture-independent rendering
//! shared across the sqlx backends.

#[cfg(any(feature = "postgres", feature = "mysql"))]
pub(crate) mod convert;
pub(crate) mod pump;
pub(crate) mod sub_batch;
