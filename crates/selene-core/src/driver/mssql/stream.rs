//! Streaming query execution: drives a tiberius [`QueryStream`] and forwards
//! result-set events to a [`RowSink`].
//!
//! ## Multi-result-set handling
//! A single SQL batch can return several result sets (e.g. two `SELECT`s
//! separated by `;`). tiberius models this as one flat stream of [`QueryItem`]s
//! where each new [`QueryItem::Metadata`] begins a set and carries an
//! incrementing `result_index`. We use that index directly as the sink's
//! `set_index`, flushing the previous set's buffered rows and emitting
//! `on_set_end` whenever the set changes or the stream ends.
//!
//! ## Cancellation semantics
//! Cancellation is **cooperative**: we check [`CancelToken::is_cancelled`] at
//! every batch boundary (and before the first poll). On cancellation we stop
//! polling immediately and return [`CoreError::Cancelled`]. Dropping the
//! `QueryStream` (which happens as this function returns) releases tiberius'
//! borrow of the connection; the Tauri layer's hard task-abort then drops the
//! whole connection, which raises a server-side Attention to actually stop a
//! long-running query. Returning `Cancelled` (rather than a partial `Ok`) keeps
//! the contract unambiguous: a cancelled execution did not "succeed", and the
//! rows already delivered to the sink remain valid for display.
//!
//! ## Row cap
//! `opts.max_rows` caps the total rows delivered across all sets. When the cap
//! is hit we stop pulling rows and mark the outcome `truncated`. A
//! [`Flow::Stop`] returned by the sink (e.g. the UI closed the result tab) ends
//! the stream promptly as well, but is *not* treated as truncation.

use futures_util::TryStreamExt;
use tiberius::{Client, QueryItem};
use tokio::net::TcpStream;
use tokio_util::compat::Compat;

use crate::driver::{CancelToken, ExecOptions, ExecOutcome, Flow, RowSink};
use crate::error::CoreError;
use crate::value::{CellValue, Column};

use super::convert::{cell_to_value, column_to_meta};
use super::error::map_tiberius_err;

/// The concrete tiberius client type Selene uses: a TDS client over a
/// Tokio TCP stream wrapped in the futures-compat shim.
pub(crate) type TiberiusClient = Client<Compat<TcpStream>>;

/// Execute `sql` on `client`, streaming result-set events to `sink`.
///
/// Uses `simple_query` (a plain TDS batch) rather than the RPC/`sp_executesql`
/// path, because a SQL editor submits free-form batches (DDL, `USE`, multiple
/// statements) that must not be wrapped as a parameterised procedure call.
pub(crate) async fn run_query(
    client: &mut TiberiusClient,
    sql: &str,
    opts: &ExecOptions,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    // Bail out before doing any work if cancellation already fired.
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }

    let stream = client.simple_query(sql).await.map_err(map_tiberius_err)?;

    pump(stream, opts, sink, cancel).await
}

/// Execute a non-row-returning DML batch via tiberius' [`Client::execute`] (an
/// `sp_executesql` RPC) **specifically to obtain affected-row counts**, which
/// the streaming [`run_query`] path cannot surface: tiberius' `QueryStream`
/// silently drops the TDS DONE token that carries the count.
///
/// Only batches that pass the driver's countable-DML gate
/// ([`is_countable_dml_batch`]) reach here, so the rows `execute()` discards are
/// never a loss — these statements return no result set. Each per-statement
/// count is reported to the sink as a **column-less result set** (`on_meta` with
/// no columns, then `on_set_end` carrying the count) so the UI renders
/// "<n> rows affected" instead of an empty grid.
///
/// [`Client::execute`]: tiberius::Client::execute
/// [`is_countable_dml_batch`]: crate::guard::is_countable_dml_batch
pub(crate) async fn run_exec_counting(
    client: &mut TiberiusClient,
    sql: &str,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    // Cancellation is best-effort on this path: `execute()` is a single
    // round-trip with no intra-statement yield point, so we honour a pre-fire
    // cancel and otherwise rely on the Tauri layer's hard task-abort (which
    // drops the connection) for a long-running DML — consistent with the
    // documented v0.1 cooperative-cancellation model.
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }

    let result = client.execute(sql, &[]).await.map_err(map_tiberius_err)?;
    let rolled_back = crate::guard::is_rollback_wrapped_dml_batch(sql);
    let counts = trim_wrapper_counts(sql, result.rows_affected());

    // One column-less set per statement, carrying its affected count. If the
    // server reported none (e.g. the batch contains `SET NOCOUNT ON`), still
    // emit a single benign set so the UI shows success, not "no result set".
    let mut result_sets = 0usize;
    if counts.is_empty() {
        let _ = sink.on_meta(0, Vec::new()).await;
        let _ = sink.on_set_end(0, None).await;
        result_sets = 1;
    } else {
        for (set_index, &affected) in counts.iter().enumerate() {
            if sink.on_meta(set_index, Vec::new()).await == Flow::Stop {
                break;
            }
            if sink.on_set_end(set_index, Some(affected)).await == Flow::Stop {
                break;
            }
            result_sets = set_index + 1;
        }
    }

    Ok(ExecOutcome {
        result_sets,
        total_rows: 0,
        truncated: false,
        rolled_back,
    })
}

/// Internal driver loop, generic over the stream so it can be unit-tested
/// without a live connection if needed. Consumes the stream to completion (or
/// until cancel / row-cap / `Flow::Stop`).
async fn pump<S>(
    mut stream: S,
    opts: &ExecOptions,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError>
where
    S: futures_util::Stream<Item = Result<QueryItem, tiberius::error::Error>> + Unpin,
{
    // State for the set currently being read.
    let mut current_set: Option<usize> = None;
    let mut batch: Vec<Vec<CellValue>> = Vec::with_capacity(opts.batch_size.max(1));

    let mut total_rows: u64 = 0;
    let mut result_sets: usize = 0;
    let mut truncated = false;

    // Whether the sink asked us to stop (cancel-like, but sink-driven).
    let mut sink_stop = false;

    loop {
        // Cooperative cancellation check at every iteration boundary.
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }

        let item = match stream.try_next().await.map_err(map_tiberius_err)? {
            Some(item) => item,
            None => break, // stream exhausted
        };

        match item {
            QueryItem::Metadata(meta) => {
                let set_index = meta.result_index();

                // A new metadata after we were reading another set closes that
                // previous set: flush its tail rows and signal its end.
                let closing_prev = current_set.filter(|&prev| prev != set_index);
                if let Some(prev) = closing_prev {
                    if flush_batch(sink, prev, &mut batch).await? == Flow::Stop {
                        sink_stop = true;
                        break;
                    }
                    if sink.on_set_end(prev, None).await == Flow::Stop {
                        sink_stop = true;
                        break;
                    }
                }

                // Announce the (new) set's columns.
                let columns = meta_columns(&meta);
                result_sets = result_sets.max(set_index + 1);
                current_set = Some(set_index);

                if sink.on_meta(set_index, columns).await == Flow::Stop {
                    sink_stop = true;
                    break;
                }
            }

            QueryItem::Row(row) => {
                // tiberius guarantees a Metadata precedes any Row, so a row
                // without a known set would be a protocol violation.
                let set_index = match current_set {
                    Some(idx) => idx,
                    None => {
                        return Err(CoreError::Protocol(
                            "received a row before any result-set metadata".into(),
                        ))
                    }
                };

                // Enforce the global row cap *before* accepting the row. If we
                // have already delivered `max` rows, the arrival of this further
                // row means the server had more than the cap — so we flush the
                // tail of the current batch, mark the outcome truncated, and
                // stop. (An exact-fit result never reaches this branch, because
                // the stream ends before another Row arrives, so it is reported
                // as non-truncated.)
                if opts.max_rows.is_some_and(|max| total_rows >= max) {
                    let _ = flush_batch(sink, set_index, &mut batch).await?;
                    truncated = true;
                    break;
                }

                batch.push(convert_row(row));
                total_rows += 1;

                // Flush once a full batch has accumulated.
                if batch.len() >= opts.batch_size.max(1)
                    && flush_batch(sink, set_index, &mut batch).await? == Flow::Stop
                {
                    sink_stop = true;
                    break;
                }
            }
        }
    }

    // Flush and close the final in-progress set, unless the sink told us to
    // stop (in which case we end promptly without further callbacks).
    if !sink_stop {
        if let Some(idx) = current_set {
            // Best-effort: ignore a Stop here since we are finishing anyway.
            let _ = flush_batch(sink, idx, &mut batch).await?;
            let _ = sink.on_set_end(idx, None).await;
        }
    }

    Ok(ExecOutcome {
        result_sets,
        total_rows,
        truncated,
        // The streaming path serves rows-returning batches (incl. a read wrapped
        // in BEGIN/ROLLBACK); affected-count dry-runs go through
        // `run_exec_counting`. Nothing to flag as rolled back here.
        rolled_back: false,
    })
}

/// Build Selene columns from tiberius result metadata, assigning ordinals.
fn meta_columns(meta: &tiberius::ResultMetadata) -> Vec<Column> {
    meta.columns()
        .iter()
        .enumerate()
        .map(|(ordinal, col)| column_to_meta(col, ordinal))
        .collect()
}

/// Convert one tiberius row into a vector of neutral cell values.
fn convert_row(row: tiberius::Row) -> Vec<CellValue> {
    // `into_iter` moves the `ColumnData` cells out by value; we borrow each to
    // reuse the shared conversion path.
    row.into_iter().map(|cd| cell_to_value(&cd)).collect()
}

/// For a rollback-wrapped dry-run batch (`BEGIN TRAN; <DML …>; ROLLBACK`),
/// strip the leading/trailing zero-count entries that `BEGIN TRAN` and
/// `ROLLBACK` themselves contribute, so the UI shows only the inner DML's
/// "<n> rows affected" set(s) instead of two phantom 0-row sets framing them.
///
/// Only the first and last entries are candidates, and only when they are `0`
/// — a genuine inner DML that affected 0 rows (which can also sit in the
/// middle) is always preserved. For non-wrapped batches this is a no-op.
fn trim_wrapper_counts<'a>(sql: &str, counts: &'a [u64]) -> &'a [u64] {
    if !crate::guard::is_rollback_wrapped_dml_batch(sql) {
        return counts;
    }
    let mut start = 0;
    let mut end = counts.len();
    if start < end && counts[start] == 0 {
        start += 1;
    }
    if end > start && counts[end - 1] == 0 {
        end -= 1;
    }
    &counts[start..end]
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
    use std::sync::Arc;
    use tiberius::{Column as TiberiusColumn, ColumnType};

    // A QueryItem cannot be hand-constructed (its inner Row/ResultMetadata have
    // private fields), so the pump's stream-shaped logic is exercised here only
    // for the parts we can build: an empty stream and an error stream. Full
    // multi-result-set behaviour is covered by the integration tests added with
    // testcontainers in a later phase.

    /// Recording sink that captures the callback sequence.
    #[derive(Default)]
    struct RecordingSink {
        events: Vec<String>,
        stop_after_rows: Option<usize>,
        row_batches_seen: usize,
    }

    #[async_trait]
    impl RowSink for RecordingSink {
        async fn on_meta(&mut self, set_index: usize, columns: Vec<Column>) -> Flow {
            self.events
                .push(format!("meta:{set_index}:{}", columns.len()));
            Flow::Continue
        }
        async fn on_rows(&mut self, set_index: usize, rows: Vec<Vec<CellValue>>) -> Flow {
            self.events.push(format!("rows:{set_index}:{}", rows.len()));
            self.row_batches_seen += 1;
            match self.stop_after_rows {
                Some(n) if self.row_batches_seen >= n => Flow::Stop,
                _ => Flow::Continue,
            }
        }
        async fn on_set_end(&mut self, set_index: usize, affected: Option<u64>) -> Flow {
            self.events.push(format!("end:{set_index}:{affected:?}"));
            Flow::Continue
        }
    }

    fn empty_stream(
    ) -> impl futures_util::Stream<Item = Result<QueryItem, tiberius::error::Error>> + Unpin {
        stream::iter(Vec::<Result<QueryItem, tiberius::error::Error>>::new())
    }

    #[tokio::test]
    async fn empty_stream_yields_empty_outcome() {
        let mut sink = RecordingSink::default();
        let opts = ExecOptions::default();
        let cancel = CancelToken::new();

        let outcome = pump(empty_stream(), &opts, &mut sink, &cancel)
            .await
            .expect("empty stream pumps cleanly");

        assert_eq!(outcome.total_rows, 0);
        assert_eq!(outcome.result_sets, 0);
        assert!(!outcome.truncated);
        assert!(sink.events.is_empty());
    }

    #[tokio::test]
    async fn precancelled_token_short_circuits() {
        let mut sink = RecordingSink::default();
        let opts = ExecOptions::default();
        let cancel = CancelToken::new();
        cancel.cancel();

        let err = pump(empty_stream(), &opts, &mut sink, &cancel)
            .await
            .expect_err("cancelled before first poll");
        assert!(matches!(err, CoreError::Cancelled));
    }

    #[tokio::test]
    async fn stream_error_maps_to_core_error() {
        let mut sink = RecordingSink::default();
        let opts = ExecOptions::default();
        let cancel = CancelToken::new();

        let err_item: Result<QueryItem, tiberius::error::Error> =
            Err(tiberius::error::Error::Protocol("boom".into()));
        let s = stream::iter(vec![err_item]);

        let err = pump(Box::pin(s), &opts, &mut sink, &cancel)
            .await
            .expect_err("protocol error propagates");
        assert!(matches!(err, CoreError::Protocol(_)));
    }

    // Sanity check that the public column conversion produces sane metadata; it
    // is the same path `meta_columns` uses per column.
    #[test]
    fn column_meta_has_ordinals_and_logical_types() {
        let cols = [
            TiberiusColumn::new("id".to_string(), ColumnType::Int4),
            TiberiusColumn::new("name".to_string(), ColumnType::NVarchar),
        ];
        let converted: Vec<Column> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| column_to_meta(c, i))
            .collect();

        assert_eq!(converted[0].name, "id");
        assert_eq!(converted[0].ordinal, 0);
        assert_eq!(converted[0].logical, LogicalType::Integer);
        assert_eq!(converted[1].name, "name");
        assert_eq!(converted[1].ordinal, 1);
        assert_eq!(converted[1].logical, LogicalType::Text);
        // Keep Arc import used in case future tests need shared columns.
        let _ = Arc::new(0u8);
    }

    // --- trim_wrapper_counts ------------------------------------------------

    #[test]
    fn trim_wrapper_counts_strips_begin_and_rollback_zeros() {
        // Typical dry-run shape: [BEGIN=0, UPDATE=193, ROLLBACK=0].
        let counts = [0u64, 193, 0];
        let trimmed = trim_wrapper_counts(
            "BEGIN TRAN;\nUPDATE t SET x = 1 WHERE id = 1;\nROLLBACK;",
            &counts,
        );
        assert_eq!(trimmed, &[193]);
    }

    #[test]
    fn trim_wrapper_counts_keeps_inner_zero_dml_count() {
        // The inner UPDATE genuinely affected 0 rows — must be preserved.
        let counts = [0u64, 0, 0];
        let trimmed = trim_wrapper_counts(
            "BEGIN TRAN;\nUPDATE t SET x = 1 WHERE id = 999;\nROLLBACK;",
            &counts,
        );
        assert_eq!(trimmed, &[0]);
    }

    #[test]
    fn trim_wrapper_counts_handles_multiple_inner_dml() {
        // [BEGIN=0, UPDATE=5, DELETE=2, ROLLBACK=0] -> [5, 2].
        let counts = [0u64, 5, 2, 0];
        let trimmed = trim_wrapper_counts(
            "BEGIN TRAN;\nUPDATE a SET x = 1;\nDELETE FROM b;\nROLLBACK;",
            &counts,
        );
        assert_eq!(trimmed, &[5, 2]);
    }

    #[test]
    fn trim_wrapper_counts_handles_no_semicolon_form() {
        let counts = [0u64, 193, 0];
        let trimmed = trim_wrapper_counts(
            "BEGIN TRANSACTION\nUPDATE t SET x = 1 WHERE id = 1\nROLLBACK",
            &counts,
        );
        assert_eq!(trimmed, &[193]);
    }

    #[test]
    fn trim_wrapper_counts_is_noop_for_plain_dml() {
        // A non-wrapped DML batch must keep all counts (including a genuine 0).
        let counts = [3u64, 0, 7];
        let trimmed = trim_wrapper_counts(
            "UPDATE a SET x = 1; UPDATE b SET x = 1 WHERE id = 999; DELETE FROM c",
            &counts,
        );
        assert_eq!(trimmed, &[3, 0, 7]);
    }
}
