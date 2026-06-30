//! A generic streaming pump over sqlx's `fetch_many` output.
//!
//! sqlx's [`Executor::fetch_many`](sqlx::Executor::fetch_many) yields a flat
//! stream of `Result<Either<QueryResult, Row>, sqlx::Error>`:
//! - `Either::Right(row)` is one data row;
//! - `Either::Left(query_result)` ends a statement and carries its
//!   `rows_affected()` (DML count).
//!
//! This is the sqlx analogue of the mssql driver's `stream::pump`. It batches
//! rows to a [`RowSink`], enforces the row cap and cooperative cancellation, and
//! splits the flat stream into result sets — but it is generic over the backend
//! row/query-result types via closures, so SQLite (and later Postgres/MySQL)
//! reuse it unchanged.
//!
//! ## Result-set boundaries
//! sqlx does not number result sets for us, so we derive boundaries from the
//! shape of the stream:
//! - A **row whose column set differs** from the set currently being read starts
//!   a new set (the previous set is flushed and closed first). This is what
//!   separates two `SELECT`s in one batch.
//! - A `Left(query_result)` that **followed rows** closes the current row-set
//!   (its affected count is reported on that set's `on_set_end`), and the next
//!   row — if any — starts a fresh set.
//! - A `Left(query_result)` with **no rows in the current statement** is a DML
//!   statement (INSERT/UPDATE/DELETE): we emit a column-less set (`on_meta` with
//!   no columns, then `on_set_end` carrying the affected count) so the UI shows
//!   "<n> rows affected" rather than nothing.
//!
//! ## Cancellation & row cap
//! Cancellation is cooperative: [`CancelToken::is_cancelled`] is checked at every
//! stream-item boundary and a tripped token returns [`CoreError::Cancelled`].
//! `opts.max_rows` caps the total rows across all sets; hitting it flushes the
//! current batch, marks the outcome `truncated`, and stops. A [`Flow::Stop`]
//! from the sink ends the stream promptly (not treated as truncation).

use futures_util::stream::Stream;
use futures_util::TryStreamExt;

use crate::driver::{CancelToken, ExecOptions, ExecOutcome, Flow, RowSink};
use crate::error::CoreError;
use crate::value::{CellValue, Column};

/// One half of a `fetch_many` stream item, re-exported through `sqlx::Either`.
/// Aliased here so callers and tests name it without importing `sqlx` directly.
pub(crate) type RowOrResult<QueryResult, Row> = sqlx::Either<QueryResult, Row>;

/// Drive a sqlx `fetch_many` stream into `sink`, returning an [`ExecOutcome`].
///
/// The backend supplies three closures:
/// - `columns_of`: a row's column metadata (used to announce a set and to detect
///   when the column set changes between rows);
/// - `convert_row`: a row's cells as neutral [`CellValue`]s;
/// - `affected`: a query result's affected-row count.
///
/// `map_err` maps a `sqlx::Error` to a [`CoreError`] (each driver passes its own
/// `error::map_sqlx_err`).
// The backend hooks (three closures + the error mapper) are intrinsic to making
// one pump serve every sqlx driver; bundling them into a struct would only move
// the parameters around, so the lint is allowed here deliberately.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pump<S, QueryResult, Row>(
    mut stream: S,
    opts: &ExecOptions,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
    columns_of: impl Fn(&Row) -> Vec<Column>,
    convert_row: impl Fn(&Row) -> Vec<CellValue>,
    affected: impl Fn(&QueryResult) -> u64,
    map_err: impl Fn(sqlx::Error) -> CoreError,
) -> Result<ExecOutcome, CoreError>
where
    S: Stream<Item = Result<RowOrResult<QueryResult, Row>, sqlx::Error>> + Unpin,
{
    // Bail before any work if cancellation already fired.
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }

    // The set currently being read: its index, its column metadata (to detect a
    // change), and how many rows we've buffered/seen for the current statement.
    let mut current_set: Option<usize> = None;
    let mut current_columns: Vec<Column> = Vec::new();
    let mut rows_in_stmt: u64 = 0;
    let mut batch: Vec<Vec<CellValue>> = Vec::with_capacity(opts.batch_size.max(1));

    // The next set index to assign. Incremented each time a set opens.
    let mut next_index: usize = 0;

    let mut total_rows: u64 = 0;
    let mut result_sets: usize = 0;
    let mut truncated = false;
    let mut sink_stop = false;

    loop {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }

        let item = match stream.try_next().await.map_err(&map_err)? {
            Some(item) => item,
            None => break, // stream exhausted
        };

        match item {
            RowOrResult::Right(row) => {
                let cols = columns_of(&row);

                // Decide whether this row belongs to the current set or starts a
                // new one. A new set begins when there is no open set, or when
                // the column set differs from the open set's columns.
                let starts_new = match current_set {
                    None => true,
                    Some(_) => cols != current_columns,
                };

                if starts_new {
                    // Close the previous row-set, if any, before opening a new one.
                    if let Some(prev) = current_set {
                        if flush_batch(sink, prev, &mut batch).await? == Flow::Stop {
                            sink_stop = true;
                            break;
                        }
                        if sink.on_set_end(prev, None).await == Flow::Stop {
                            sink_stop = true;
                            break;
                        }
                    }

                    let idx = next_index;
                    next_index += 1;
                    result_sets = result_sets.max(idx + 1);
                    current_set = Some(idx);
                    current_columns = cols.clone();
                    rows_in_stmt = 0;

                    if sink.on_meta(idx, cols).await == Flow::Stop {
                        sink_stop = true;
                        break;
                    }
                }

                let set_index = current_set.expect("set opened above");

                // Enforce the global row cap *before* accepting the row: an extra
                // row beyond the cap means the server had more than `max`, so we
                // flush the tail, mark truncated, and stop. (An exact-fit result
                // never reaches here — the stream ends first — so it is reported
                // as non-truncated.)
                if opts.max_rows.is_some_and(|max| total_rows >= max) {
                    let _ = flush_batch(sink, set_index, &mut batch).await?;
                    truncated = true;
                    break;
                }

                batch.push(convert_row(&row));
                total_rows += 1;
                rows_in_stmt += 1;

                if batch.len() >= opts.batch_size.max(1)
                    && flush_batch(sink, set_index, &mut batch).await? == Flow::Stop
                {
                    sink_stop = true;
                    break;
                }
            }

            RowOrResult::Left(query_result) => {
                let affected_rows = affected(&query_result);

                match current_set {
                    // A query result that followed rows closes that row-set,
                    // reporting its affected count. The next row (if any) opens a
                    // fresh set.
                    Some(prev) if rows_in_stmt > 0 => {
                        if flush_batch(sink, prev, &mut batch).await? == Flow::Stop {
                            sink_stop = true;
                            break;
                        }
                        if sink.on_set_end(prev, Some(affected_rows)).await == Flow::Stop {
                            sink_stop = true;
                            break;
                        }
                        current_set = None;
                        current_columns.clear();
                        rows_in_stmt = 0;
                    }
                    // A query result with no rows in the current statement is a
                    // DML statement: emit a column-less set carrying the count.
                    _ => {
                        let idx = next_index;
                        next_index += 1;
                        result_sets = result_sets.max(idx + 1);

                        if sink.on_meta(idx, Vec::new()).await == Flow::Stop {
                            sink_stop = true;
                            break;
                        }
                        if sink.on_set_end(idx, Some(affected_rows)).await == Flow::Stop {
                            sink_stop = true;
                            break;
                        }
                        // The column-less set is fully emitted; reset state.
                        current_set = None;
                        current_columns.clear();
                        rows_in_stmt = 0;
                    }
                }
            }
        }
    }

    // Flush and close any final in-progress row-set, unless the sink stopped us.
    if !sink_stop {
        if let Some(idx) = current_set {
            let _ = flush_batch(sink, idx, &mut batch).await?;
            let _ = sink.on_set_end(idx, None).await;
        }
    }

    Ok(ExecOutcome {
        result_sets,
        total_rows,
        truncated,
        // sqlx drivers don't run the mssql rollback-wrapped dry-run path, so a
        // streamed batch is never flagged as rolled back here.
        rolled_back: false,
    })
}

/// Flush the buffered batch (if any) to the sink, clearing it. Returns the
/// sink's flow signal.
async fn flush_batch(
    sink: &mut dyn RowSink,
    set_index: usize,
    batch: &mut Vec<Vec<CellValue>>,
) -> Result<Flow, CoreError> {
    if batch.is_empty() {
        return Ok(Flow::Continue);
    }
    let rows = std::mem::take(batch);
    Ok(sink.on_rows(set_index, rows).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::LogicalType;
    use async_trait::async_trait;
    use futures_util::stream;

    /// A trivial fake row: just its columns and cells. Lets us build a
    /// `fetch_many`-shaped stream by hand and assert the pump's set-splitting,
    /// batching, cap, and DML-count behaviour without a live database.
    #[derive(Clone)]
    struct FakeRow {
        columns: Vec<Column>,
        cells: Vec<CellValue>,
    }

    /// A fake query result carrying only an affected-row count.
    #[derive(Clone, Copy)]
    struct FakeQr {
        affected: u64,
    }

    type Item = Result<RowOrResult<FakeQr, FakeRow>, sqlx::Error>;

    fn col(name: &str) -> Column {
        Column {
            name: name.to_string(),
            ordinal: 0,
            db_type: "INTEGER".to_string(),
            logical: LogicalType::Integer,
            nullable: None,
        }
    }

    fn row(cols: &[&str], vals: &[i64]) -> FakeRow {
        FakeRow {
            columns: cols.iter().map(|n| col(n)).collect(),
            cells: vals.iter().map(|&v| CellValue::I64(v)).collect(),
        }
    }

    /// Run the pump over a hand-built stream with the fake closures.
    async fn run(items: Vec<Item>, opts: ExecOptions) -> (ExecOutcome, RecordingSink) {
        let mut sink = RecordingSink::default();
        let cancel = CancelToken::new();
        let outcome = pump(
            stream::iter(items),
            &opts,
            &mut sink,
            &cancel,
            |r: &FakeRow| r.columns.clone(),
            |r: &FakeRow| r.cells.clone(),
            |q: &FakeQr| q.affected,
            |_e: sqlx::Error| CoreError::Protocol("unexpected".into()),
        )
        .await
        .expect("pump succeeds");
        (outcome, sink)
    }

    #[derive(Default)]
    struct RecordingSink {
        events: Vec<String>,
        total_rows: usize,
    }

    #[async_trait]
    impl RowSink for RecordingSink {
        async fn on_meta(&mut self, set_index: usize, columns: Vec<Column>) -> Flow {
            self.events
                .push(format!("meta:{set_index}:{}", columns.len()));
            Flow::Continue
        }
        async fn on_rows(&mut self, set_index: usize, rows: Vec<Vec<CellValue>>) -> Flow {
            self.total_rows += rows.len();
            self.events.push(format!("rows:{set_index}:{}", rows.len()));
            Flow::Continue
        }
        async fn on_set_end(&mut self, set_index: usize, affected: Option<u64>) -> Flow {
            self.events.push(format!("end:{set_index}:{affected:?}"));
            Flow::Continue
        }
    }

    #[tokio::test]
    async fn precancelled_token_short_circuits() {
        let mut sink = RecordingSink::default();
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = pump(
            stream::iter(Vec::<Item>::new()),
            &ExecOptions::default(),
            &mut sink,
            &cancel,
            |r: &FakeRow| r.columns.clone(),
            |r: &FakeRow| r.cells.clone(),
            |q: &FakeQr| q.affected,
            |_e: sqlx::Error| CoreError::Protocol("x".into()),
        )
        .await
        .expect_err("cancelled before first poll");
        assert!(matches!(err, CoreError::Cancelled));
    }

    #[tokio::test]
    async fn single_select_one_set() {
        let items = vec![
            Ok(RowOrResult::Right(row(&["id"], &[1]))),
            Ok(RowOrResult::Right(row(&["id"], &[2]))),
            Ok(RowOrResult::Left(FakeQr { affected: 0 })),
        ];
        let (outcome, sink) = run(items, ExecOptions::default()).await;
        assert_eq!(outcome.result_sets, 1);
        assert_eq!(outcome.total_rows, 2);
        assert!(!outcome.truncated);
        // One meta, rows, and a set-end carrying the trailing result's count.
        assert_eq!(
            sink.events,
            vec!["meta:0:1", "rows:0:2", "end:0:Some(0)"],
            "a SELECT's trailing query-result closes the set with its count"
        );
    }

    #[tokio::test]
    async fn two_selects_split_on_column_change() {
        let items = vec![
            Ok(RowOrResult::Right(row(&["a"], &[1]))),
            // Different column set => a new result set.
            Ok(RowOrResult::Right(row(&["b", "c"], &[2, 3]))),
        ];
        let (outcome, sink) = run(items, ExecOptions::default()).await;
        assert_eq!(outcome.result_sets, 2);
        assert_eq!(outcome.total_rows, 2);
        assert_eq!(
            sink.events,
            vec![
                "meta:0:1",
                "rows:0:1",
                "end:0:None",
                "meta:1:2",
                "rows:1:1",
                "end:1:None",
            ]
        );
    }

    #[tokio::test]
    async fn dml_only_emits_columnless_count_set() {
        // No rows, just a query result: an INSERT/UPDATE/DELETE.
        let items = vec![Ok(RowOrResult::Left(FakeQr { affected: 5 }))];
        let (outcome, sink) = run(items, ExecOptions::default()).await;
        assert_eq!(outcome.result_sets, 1);
        assert_eq!(outcome.total_rows, 0);
        assert_eq!(sink.events, vec!["meta:0:0", "end:0:Some(5)"]);
    }

    #[tokio::test]
    async fn select_then_dml_two_sets() {
        let items = vec![
            Ok(RowOrResult::Right(row(&["id"], &[1]))),
            Ok(RowOrResult::Left(FakeQr { affected: 0 })), // ends the SELECT set
            Ok(RowOrResult::Left(FakeQr { affected: 7 })), // a following DML
        ];
        let (outcome, sink) = run(items, ExecOptions::default()).await;
        assert_eq!(outcome.result_sets, 2);
        assert_eq!(outcome.total_rows, 1);
        assert_eq!(
            sink.events,
            vec![
                "meta:0:1",
                "rows:0:1",
                "end:0:Some(0)",
                "meta:1:0",
                "end:1:Some(7)"
            ]
        );
    }

    #[tokio::test]
    async fn max_rows_truncates() {
        let items = vec![
            Ok(RowOrResult::Right(row(&["id"], &[1]))),
            Ok(RowOrResult::Right(row(&["id"], &[2]))),
            Ok(RowOrResult::Right(row(&["id"], &[3]))),
        ];
        let opts = ExecOptions {
            max_rows: Some(2),
            batch_size: 1,
        };
        let (outcome, sink) = run(items, opts).await;
        assert!(outcome.truncated);
        assert_eq!(outcome.total_rows, 2);
        assert_eq!(sink.total_rows, 2);
    }

    #[tokio::test]
    async fn stream_error_maps_through_map_err() {
        let mut sink = RecordingSink::default();
        let cancel = CancelToken::new();
        let items: Vec<Item> = vec![Err(sqlx::Error::RowNotFound)];
        let err = pump(
            stream::iter(items),
            &ExecOptions::default(),
            &mut sink,
            &cancel,
            |r: &FakeRow| r.columns.clone(),
            |r: &FakeRow| r.cells.clone(),
            |q: &FakeQr| q.affected,
            |_e: sqlx::Error| CoreError::Query("mapped".into()),
        )
        .await
        .expect_err("stream error propagates");
        assert!(matches!(err, CoreError::Query(m) if m == "mapped"));
    }
}
