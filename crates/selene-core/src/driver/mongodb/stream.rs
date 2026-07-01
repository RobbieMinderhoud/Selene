//! Streaming a MongoDB read query's results into a [`RowSink`].
//!
//! MongoDB has no fixed result-set shape: a `find`/`aggregate` yields a cursor of
//! heterogeneous [`Document`]s, so before we can announce columns we must *derive*
//! them by sampling. The pump therefore differs from the SQL drivers' shared
//! `pump` (which is handed server-declared column metadata up front):
//!
//! 1. **Sample & derive columns.** Buffer up to [`COLUMN_SAMPLE`] leading
//!    documents, union their top-level field names in first-seen order (forcing
//!    `_id` to ordinal 0 when present, matching how mongosh lists it first), and
//!    infer each column's `db_type`/`logical` from the first *non-null* value
//!    seen for that key across the sample.
//! 2. **Emit `on_meta`**, then flush the buffered documents as rows aligned to
//!    the derived column order (a missing key → [`CellValue::Null`]).
//! 3. **Continue streaming** the rest, padding each document to the fixed column
//!    set. Cancellation is checked and `batch_size` flushed at every batch
//!    boundary; `max_rows` caps the total and sets `truncated` when the source
//!    had more.
//!
//! **v1 limitation:** a field that first appears in a document *after* the
//! column sample is **dropped** from that (and every) row, because the column set
//! is fixed once derived. Widening the sample (or re-emitting metadata mid-stream)
//! is deferred; [`COLUMN_SAMPLE`] is generous enough that this is rare in practice.

use bson::{Bson, Document};
use futures_util::TryStreamExt;
use mongodb::Cursor;

use crate::driver::{CancelToken, ExecOptions, ExecOutcome, Flow, RowSink};
use crate::error::CoreError;
use crate::value::{CellValue, Column, LogicalType};

use super::convert::{bson_to_cell, bson_type_name, logical_for};
use super::error::map_mongo_err;

/// How many leading documents to buffer to derive the column set. Larger than a
/// typical `batch_size` so columns are stable even when early documents omit
/// fields, but bounded so a huge result set does not balloon memory before the
/// first rows reach the UI.
const COLUMN_SAMPLE: usize = 100;

/// Stream a `find`/`aggregate` cursor into `sink`, deriving columns by sampling.
///
/// `cancel` is honoured cooperatively at every batch boundary (and once before
/// the first read). `opts.max_rows` caps the delivered rows; when the cursor
/// still had documents past the cap the outcome is marked `truncated`.
pub(crate) async fn stream_cursor(
    mut cursor: Cursor<Document>,
    opts: &ExecOptions,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }

    let batch_size = opts.batch_size.max(1);

    // --- Phase 1: buffer a sample to derive columns. -----------------------
    // We buffer up to COLUMN_SAMPLE docs (but never more than we're allowed to
    // deliver under max_rows) so the derived column set is stable before the
    // first on_meta.
    let mut sample: Vec<Document> = Vec::new();
    let mut truncated = false;
    // Whether the cursor is exhausted after filling the sample.
    let mut cursor_done = false;

    loop {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        // Stop sampling once we've hit the sample cap or the row cap.
        let cap = sample_cap(opts.max_rows);
        if sample.len() >= cap {
            break;
        }
        match cursor.try_next().await.map_err(map_mongo_err)? {
            Some(doc) => sample.push(doc),
            None => {
                cursor_done = true;
                break;
            }
        }
    }

    let columns = derive_columns(&sample);

    if sink.on_meta(0, columns.clone()).await == Flow::Stop {
        // The sink bailed before any rows; still report the (empty) set cleanly.
        let _ = sink.on_set_end(0, None).await;
        return Ok(ExecOutcome {
            result_sets: 1,
            total_rows: 0,
            truncated: false,
            rolled_back: false,
        });
    }

    // Zero documents: emit an empty grid (meta already sent) and end the set.
    // This is a successful empty result, not an error.
    if sample.is_empty() && cursor_done {
        let _ = sink.on_set_end(0, Some(0)).await;
        return Ok(ExecOutcome {
            result_sets: 1,
            total_rows: 0,
            truncated: false,
            rolled_back: false,
        });
    }

    let mut total_rows: u64 = 0;
    let mut sink_stopped = false;

    // --- Phase 2: flush the sampled documents as rows. ---------------------
    let mut batch: Vec<Vec<CellValue>> = Vec::with_capacity(batch_size);
    for doc in sample.drain(..) {
        // The sample was already capped at max_rows, so every sampled doc is
        // deliverable — no cap check needed here.
        batch.push(row_for(&doc, &columns));
        total_rows += 1;
        if batch.len() >= batch_size && flush(sink, &mut batch).await? == Flow::Stop {
            sink_stopped = true;
            break;
        }
    }

    // --- Phase 3: stream the remainder past the sample. --------------------
    if !sink_stopped && !cursor_done {
        loop {
            if cancel.is_cancelled() {
                return Err(CoreError::Cancelled);
            }

            // Enforce the row cap *before* accepting the next document: if we've
            // already delivered `max` rows and the cursor still yields one, the
            // source had more than the cap — flush the tail, mark truncated, stop.
            if opts.max_rows.is_some_and(|max| total_rows >= max) {
                // Peek one more document to decide truncation. If there is one,
                // the result was genuinely truncated; if not, it fit exactly.
                if cursor.try_next().await.map_err(map_mongo_err)?.is_some() {
                    truncated = true;
                }
                break;
            }

            match cursor.try_next().await.map_err(map_mongo_err)? {
                Some(doc) => {
                    batch.push(row_for(&doc, &columns));
                    total_rows += 1;
                    if batch.len() >= batch_size && flush(sink, &mut batch).await? == Flow::Stop {
                        sink_stopped = true;
                        break;
                    }
                }
                None => break, // cursor exhausted
            }
        }
    }

    // Flush the tail and close the set, unless the sink told us to stop.
    if !sink_stopped {
        let _ = flush(sink, &mut batch).await?;
        let _ = sink.on_set_end(0, None).await;
    }

    Ok(ExecOutcome {
        result_sets: 1,
        total_rows,
        truncated,
        rolled_back: false,
    })
}

/// Deliver a `countDocuments`/`count` result: a single-column (`count`) set with
/// one integer row.
pub(crate) async fn emit_count(
    count: u64,
    sink: &mut dyn RowSink,
) -> Result<ExecOutcome, CoreError> {
    let columns = vec![Column {
        name: "count".to_string(),
        ordinal: 0,
        db_type: "long".to_string(),
        logical: LogicalType::Integer,
        nullable: Some(false),
    }];
    if sink.on_meta(0, columns).await == Flow::Stop {
        let _ = sink.on_set_end(0, Some(0)).await;
        return Ok(ExecOutcome {
            result_sets: 1,
            total_rows: 0,
            truncated: false,
            rolled_back: false,
        });
    }
    // `count_documents` returns u64; carry it as i64 (our neutral integer). A
    // count exceeding i64::MAX is not physically reachable.
    let value = i64::try_from(count).unwrap_or(i64::MAX);
    if sink.on_rows(0, vec![vec![CellValue::I64(value)]]).await != Flow::Stop {
        let _ = sink.on_set_end(0, Some(1)).await;
    }
    Ok(ExecOutcome {
        result_sets: 1,
        total_rows: 1,
        truncated: false,
        rolled_back: false,
    })
}

/// Deliver a `distinct` result: a single-column set named after the field (or
/// `"value"` when the field name is empty), one row per distinct value, honouring
/// `max_rows`.
pub(crate) async fn emit_distinct(
    field: &str,
    values: Vec<Bson>,
    opts: &ExecOptions,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }

    let name = if field.is_empty() { "value" } else { field };
    // Infer the column type from the first non-null distinct value; distinct
    // values of one field are usually homogeneous.
    let (db_type, logical) = values
        .iter()
        .find(|v| !matches!(v, Bson::Null))
        .map(|v| (bson_type_name(v).to_string(), logical_for(v)))
        .unwrap_or_else(|| ("mixed".to_string(), LogicalType::Other));

    let columns = vec![Column {
        name: name.to_string(),
        ordinal: 0,
        db_type,
        logical,
        nullable: None,
    }];
    if sink.on_meta(0, columns).await == Flow::Stop {
        let _ = sink.on_set_end(0, None).await;
        return Ok(ExecOutcome {
            result_sets: 1,
            total_rows: 0,
            truncated: false,
            rolled_back: false,
        });
    }

    let cap = opts.max_rows;
    let total_available = values.len() as u64;
    let mut total_rows: u64 = 0;
    let batch_size = opts.batch_size.max(1);
    let mut batch: Vec<Vec<CellValue>> = Vec::with_capacity(batch_size);
    let mut sink_stopped = false;

    for value in values {
        if cap.is_some_and(|max| total_rows >= max) {
            break;
        }
        batch.push(vec![bson_to_cell(&value)]);
        total_rows += 1;
        if batch.len() >= batch_size && flush(sink, &mut batch).await? == Flow::Stop {
            sink_stopped = true;
            break;
        }
    }

    if !sink_stopped {
        let _ = flush(sink, &mut batch).await?;
        let _ = sink.on_set_end(0, Some(total_rows)).await;
    }

    Ok(ExecOutcome {
        result_sets: 1,
        total_rows,
        truncated: cap.is_some_and(|max| total_available > max),
        rolled_back: false,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// How many documents to buffer for the column sample: the smaller of the
/// [`COLUMN_SAMPLE`] cap and the row cap (there is no point sampling rows we are
/// not allowed to deliver).
fn sample_cap(max_rows: Option<u64>) -> usize {
    match max_rows {
        Some(max) => {
            let max = usize::try_from(max).unwrap_or(usize::MAX);
            COLUMN_SAMPLE.min(max)
        }
        None => COLUMN_SAMPLE,
    }
}

/// Derive the fixed column set from a sample of documents.
///
/// The union of top-level field names is taken in first-seen order, but `_id`
/// (present on almost every MongoDB document) is forced to ordinal 0 so it reads
/// first, matching mongosh. Each column's type is inferred from the first
/// *non-null* value seen for that key; a key seen only as null (or never with a
/// value) falls back to a neutral `"mixed"`/`Other`.
fn derive_columns(sample: &[Document]) -> Vec<Column> {
    // First-seen field order, with `_id` pinned to the front.
    let mut order: Vec<String> = Vec::new();
    let mut has_id = false;
    for doc in sample {
        for key in doc.keys() {
            if key == "_id" {
                has_id = true;
                continue;
            }
            if !order.iter().any(|k| k == key) {
                order.push(key.clone());
            }
        }
    }
    if has_id {
        order.insert(0, "_id".to_string());
    }

    order
        .into_iter()
        .enumerate()
        .map(|(ordinal, name)| {
            // Infer the type from the first non-null value across the sample.
            let typed = sample
                .iter()
                .filter_map(|doc| doc.get(&name))
                .find(|v| !matches!(v, Bson::Null));
            let (db_type, logical) = match typed {
                Some(v) => (bson_type_name(v).to_string(), logical_for(v)),
                None => ("mixed".to_string(), LogicalType::Other),
            };
            Column {
                name,
                ordinal,
                db_type,
                logical,
                // MongoDB documents carry no per-field nullability declaration.
                nullable: None,
            }
        })
        .collect()
}

/// Build one row aligned to `columns`: a cell per column, with a missing key
/// rendered as [`CellValue::Null`]. Keys not in `columns` (i.e. fields that first
/// appeared after the sample) are dropped — the documented v1 limitation.
fn row_for(doc: &Document, columns: &[Column]) -> Vec<CellValue> {
    columns
        .iter()
        .map(|col| match doc.get(&col.name) {
            Some(value) => bson_to_cell(value),
            None => CellValue::Null,
        })
        .collect()
}

/// Flush the buffered batch (if any) to the sink, clearing it. Returns the
/// sink's flow signal.
async fn flush(sink: &mut dyn RowSink, batch: &mut Vec<Vec<CellValue>>) -> Result<Flow, CoreError> {
    if batch.is_empty() {
        return Ok(Flow::Continue);
    }
    let rows = std::mem::take(batch);
    Ok(sink.on_rows(0, rows).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn columns_union_in_first_seen_order_with_id_first() {
        let sample = vec![
            doc! { "name": "a", "qty": 1 },
            doc! { "_id": 1, "name": "b", "extra": true },
        ];
        let cols = derive_columns(&sample);
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        // `_id` is pinned first even though it appeared in the second doc.
        assert_eq!(names, vec!["_id", "name", "qty", "extra"]);
        // Ordinals are dense and start at 0.
        for (i, c) in cols.iter().enumerate() {
            assert_eq!(c.ordinal, i);
        }
    }

    #[test]
    fn column_type_inferred_from_first_non_null() {
        let sample = vec![doc! { "x": Bson::Null }, doc! { "x": 42i32 }];
        let cols = derive_columns(&sample);
        let x = cols.iter().find(|c| c.name == "x").unwrap();
        assert_eq!(x.db_type, "int");
        assert_eq!(x.logical, LogicalType::Integer);
    }

    #[test]
    fn column_all_null_falls_back_to_mixed() {
        let sample = vec![doc! { "x": Bson::Null }];
        let cols = derive_columns(&sample);
        let x = &cols[0];
        assert_eq!(x.db_type, "mixed");
        assert_eq!(x.logical, LogicalType::Other);
    }

    #[test]
    fn row_aligns_and_pads_missing_keys() {
        let cols = derive_columns(&[doc! { "_id": 1, "a": "x", "b": 2i32 }]);
        // A doc missing `b` pads that cell with Null; a field not in cols is dropped.
        let row = row_for(&doc! { "_id": 5, "a": "y", "c": "dropped" }, &cols);
        assert_eq!(row.len(), cols.len());
        assert_eq!(row[0], CellValue::I64(5)); // _id
        assert_eq!(row[1], CellValue::String("y".into())); // a
        assert_eq!(row[2], CellValue::Null); // b missing → Null
    }

    #[test]
    fn sample_cap_respects_max_rows() {
        assert_eq!(sample_cap(None), COLUMN_SAMPLE);
        assert_eq!(sample_cap(Some(10)), 10);
        assert_eq!(sample_cap(Some(1_000)), COLUMN_SAMPLE);
    }
}
