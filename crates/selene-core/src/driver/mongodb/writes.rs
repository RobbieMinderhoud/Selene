//! Executing the MongoDB **write** methods (insert/update/delete/replace/drop).
//!
//! A MongoDB write is a single server round-trip that returns a count, not a
//! cursor of rows. So — mirroring how the SQL drivers surface DML — each write
//! emits a **column-less affected-count result set**: `on_meta(0, [])` (no
//! columns), then `on_set_end(0, Some(count))`. The UI renders that as
//! "`<n>` documents affected" rather than an empty grid.
//!
//! ## What "affected" means per method
//! - **insertOne**: 1 (the one inserted document).
//! - **insertMany**: the number of inserted ids.
//! - **updateOne / updateMany**: `modified_count`, plus 1 when an `upsert`
//!   *inserted* a new document (`upserted_id.is_some()`) — a matched-but-unchanged
//!   document contributes 0, matching MongoDB's own accounting.
//! - **deleteOne / deleteMany**: `deleted_count`.
//! - **replaceOne**: `modified_count` (+ 1 on an upsert insert), like update.
//! - **drop**: 0 — dropping a collection returns no count; we report `Some(0)`
//!   so the UI shows a clean "0 documents affected" success rather than nothing.
//!
//! Cancellation: writes are single round-trips with no batch boundary to poll,
//! so the token is honoured by a single up-front check (a token that fired before
//! we started aborts the write).

use bson::{Bson, Document};
use mongodb::Collection;

use crate::driver::{CancelToken, ExecOutcome, Flow, RowSink};
use crate::error::CoreError;

use super::error::map_mongo_err;

/// Emit a single column-less affected-count result set carrying `count`, exactly
/// as the SQL drivers surface a DML statement's affected-row count.
async fn emit_affected(count: u64, sink: &mut dyn RowSink) -> Result<ExecOutcome, CoreError> {
    // A DML-style set: metadata with no columns, then the count on set-end.
    if sink.on_meta(0, Vec::new()).await != Flow::Stop {
        let _ = sink.on_set_end(0, Some(count)).await;
    }
    Ok(ExecOutcome {
        result_sets: 1,
        // No rows are delivered; the count rides on `on_set_end`.
        total_rows: 0,
        truncated: false,
        rolled_back: false,
    })
}

/// Guard: refuse to start a write if the token already fired.
fn check_cancel(cancel: &CancelToken) -> Result<(), CoreError> {
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    Ok(())
}

/// `insertOne(doc)` — insert one document, affected = 1.
pub(crate) async fn insert_one(
    coll: Collection<Document>,
    document: Document,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    check_cancel(cancel)?;
    coll.insert_one(document).await.map_err(map_mongo_err)?;
    emit_affected(1, sink).await
}

/// `insertMany([docs])` — affected = number of inserted ids.
pub(crate) async fn insert_many(
    coll: Collection<Document>,
    documents: Vec<Document>,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    check_cancel(cancel)?;
    let result = coll.insert_many(documents).await.map_err(map_mongo_err)?;
    emit_affected(result.inserted_ids.len() as u64, sink).await
}

/// `updateOne(filter, update).upsert(upsert)` — affected = modified + upsert-insert.
pub(crate) async fn update_one(
    coll: Collection<Document>,
    filter: Document,
    update: Document,
    upsert: bool,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    check_cancel(cancel)?;
    let result = coll
        .update_one(filter, update)
        .upsert(upsert)
        .await
        .map_err(map_mongo_err)?;
    emit_affected(
        modified_plus_upsert(result.modified_count, result.upserted_id),
        sink,
    )
    .await
}

/// `updateMany(filter, update).upsert(upsert)` — affected = modified + upsert-insert.
pub(crate) async fn update_many(
    coll: Collection<Document>,
    filter: Document,
    update: Document,
    upsert: bool,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    check_cancel(cancel)?;
    let result = coll
        .update_many(filter, update)
        .upsert(upsert)
        .await
        .map_err(map_mongo_err)?;
    emit_affected(
        modified_plus_upsert(result.modified_count, result.upserted_id),
        sink,
    )
    .await
}

/// `replaceOne(filter, replacement).upsert(upsert)` — affected like update.
pub(crate) async fn replace_one(
    coll: Collection<Document>,
    filter: Document,
    replacement: Document,
    upsert: bool,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    check_cancel(cancel)?;
    let result = coll
        .replace_one(filter, replacement)
        .upsert(upsert)
        .await
        .map_err(map_mongo_err)?;
    emit_affected(
        modified_plus_upsert(result.modified_count, result.upserted_id),
        sink,
    )
    .await
}

/// `deleteOne(filter)` — affected = deleted_count.
pub(crate) async fn delete_one(
    coll: Collection<Document>,
    filter: Document,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    check_cancel(cancel)?;
    let result = coll.delete_one(filter).await.map_err(map_mongo_err)?;
    emit_affected(result.deleted_count, sink).await
}

/// `deleteMany(filter)` — affected = deleted_count.
pub(crate) async fn delete_many(
    coll: Collection<Document>,
    filter: Document,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    check_cancel(cancel)?;
    let result = coll.delete_many(filter).await.map_err(map_mongo_err)?;
    emit_affected(result.deleted_count, sink).await
}

/// `drop()` — drop the collection. It returns no count, so report 0 affected.
pub(crate) async fn drop_collection(
    coll: Collection<Document>,
    sink: &mut dyn RowSink,
    cancel: &CancelToken,
) -> Result<ExecOutcome, CoreError> {
    check_cancel(cancel)?;
    coll.drop().await.map_err(map_mongo_err)?;
    emit_affected(0, sink).await
}

/// The affected count for an update/replace: `modified_count`, plus 1 when an
/// `upsert` inserted a brand-new document (a matched-but-unchanged document has
/// `modified_count == 0` and no `upserted_id`, so it correctly contributes 0).
fn modified_plus_upsert(modified_count: u64, upserted_id: Option<Bson>) -> u64 {
    modified_count + u64::from(upserted_id.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modified_plus_upsert_adds_one_only_on_insert() {
        // A plain modify: no upserted id → just the modified count.
        assert_eq!(modified_plus_upsert(3, None), 3);
        // An upsert that inserted a new document counts as one affected.
        assert_eq!(modified_plus_upsert(0, Some(Bson::Int32(1))), 1);
        // A modify *and* (hypothetically) an upserted id would sum.
        assert_eq!(modified_plus_upsert(2, Some(Bson::Int32(1))), 3);
    }
}
