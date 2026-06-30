//! Streaming query execution for SQLite.
//!
//! The user's free-form SQL is run through [`sqlx::raw_sql`] (unprepared, so a
//! batch may contain several statements and need no bind parameters), and its
//! `fetch_many` stream is driven by the shared [`pump`](crate::driver::shared::pump)
//! with SQLite-specific closures. SQLite needs none of the mssql driver's
//! special `USE`/affected-count handling — the shared pump derives result-set
//! boundaries and reports DML affected-row counts directly from the stream.
//!
//! Cancellation is cooperative (checked at each stream boundary by the pump);
//! SQLite has no server-side cancel, so a tripped token simply stops pulling and
//! returns [`CoreError::Cancelled`].

use sqlx::sqlite::SqliteConnection;
use sqlx::Executor as _;

use crate::driver::shared::pump::pump;
use crate::driver::{CancelToken, ExecOptions, ExecOutcome, RowSink};
use crate::error::CoreError;

use super::convert::{columns_of, convert_row};
use super::error::map_sqlx_err;

/// Execute `sql` on `conn`, streaming result-set events to `sink`.
pub(crate) async fn run_query(
    conn: &mut SqliteConnection,
    sql: &str,
    opts: &ExecOptions,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    // Short-circuit a pre-fired cancel before touching the connection.
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }

    // `raw_sql` runs the batch unprepared, allowing multiple statements; its
    // `fetch_many` yields the `Either<QueryResult, Row>` stream the pump drives.
    let stream = conn.fetch_many(sqlx::raw_sql(sql));

    pump(
        stream,
        opts,
        sink,
        cancel,
        columns_of,
        convert_row,
        |qr: &sqlx::sqlite::SqliteQueryResult| qr.rows_affected(),
        map_sqlx_err,
    )
    .await
}
