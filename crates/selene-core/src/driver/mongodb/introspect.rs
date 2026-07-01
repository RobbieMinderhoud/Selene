//! MongoDB introspection **by sampling**.
//!
//! MongoDB has no catalog of columns: a collection is a heap of heterogeneous
//! documents. To fit Selene's relational object tree we approximate it:
//!
//! - **Collections as tables.** `listCollections` enumerates the database's
//!   collections; a `view` (created with `viewOn`) is reported as a
//!   [`TableKind::View`], everything else as a [`TableKind::Table`]. MongoDB has
//!   no schema level, so the `schema` field is always empty.
//! - **Fields as columns, inferred from a sample.** `list_columns` samples up to
//!   [`SAMPLE_SIZE`] documents (via `$sample`, falling back to `find().limit(N)`
//!   when `$sample` is unavailable — e.g. on a view) and unions their top-level
//!   field names, forcing `_id` first (as mongosh does). Each field's `data_type`
//!   is inferred from the first non-null value seen; `nullable` reflects whether
//!   the field was ever missing or explicitly null across the sample;
//!   `is_primary_key` is set for `_id`.
//!
//! **v1 limitation:** the sample size is a fixed backend default. A field that
//! never appears in the sample is invisible, and rare types may be under-reported
//! — acceptable for a schema *hint*, which is all a schemaless store can offer.
//! Threading a user-configurable sample size through the introspection call is
//! deferred to M4.

use bson::{doc, Document};
use futures_util::TryStreamExt;
use mongodb::results::CollectionType;
use mongodb::{Client, Collection};

use crate::error::CoreError;
use crate::introspect::{ColumnInfo, TableInfo, TableKind};

use super::convert::bson_type_name;
use super::error::map_mongo_err;

/// How many documents to sample when inferring a collection's field shape. A
/// fixed backend default for v1 (see the module note); large enough that stable
/// fields are seen even when early documents omit them, bounded so introspecting
/// a huge collection stays cheap.
const SAMPLE_SIZE: usize = 100;

/// List the collections in `database` as [`TableInfo`]s.
///
/// `database` is the database name the object tree passes; an empty string falls
/// back to `default_db` (the connection's current database). MongoDB has no
/// schema level, so every `TableInfo.schema` is empty. A view (created with
/// `viewOn`, reported by the server as [`CollectionType::View`]) becomes a
/// [`TableKind::View`]; a plain collection or a timeseries becomes a
/// [`TableKind::Table`]. Results are sorted by name for a stable tree.
pub(super) async fn list_tables(
    client: &Client,
    default_db: Option<&str>,
    database: &str,
) -> Result<Vec<TableInfo>, CoreError> {
    let db_name = resolve_db(database, default_db)?;
    let db = client.database(db_name);

    let mut cursor = db.list_collections().await.map_err(map_mongo_err)?;
    let mut tables: Vec<TableInfo> = Vec::new();
    while let Some(spec) = cursor.try_next().await.map_err(map_mongo_err)? {
        // A view is either flagged by the server's `type` or (belt-and-braces)
        // carries a `viewOn` in its create options.
        let is_view =
            matches!(spec.collection_type, CollectionType::View) || spec.options.view_on.is_some();
        tables.push(TableInfo {
            schema: String::new(),
            name: spec.name,
            kind: if is_view {
                TableKind::View
            } else {
                TableKind::Table
            },
        });
    }

    // Stable order for the tree (listCollections order is unspecified).
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tables)
}

/// Infer a collection's columns by sampling documents.
///
/// Samples up to [`SAMPLE_SIZE`] documents (via `$sample`, falling back to
/// `find().limit()` when `$sample` errors — e.g. on a view, which does not
/// support it) and derives one [`ColumnInfo`] per top-level field. `_id` is
/// forced first and flagged `is_primary_key`; a field's `data_type` is inferred
/// from its first non-null value (`"mixed"` if only ever null); `nullable` is
/// true when the field was missing or explicitly null in at least one sampled
/// document. `schema` is ignored (MongoDB has no schema level).
pub(super) async fn list_columns(
    client: &Client,
    default_db: Option<&str>,
    database: &str,
    collection: &str,
) -> Result<Vec<ColumnInfo>, CoreError> {
    let db_name = resolve_db(database, default_db)?;
    let coll: Collection<Document> = client.database(db_name).collection(collection);

    let sample = sample_documents(&coll).await?;
    Ok(derive_columns(&sample))
}

/// Sample up to [`SAMPLE_SIZE`] documents from `coll`.
///
/// Prefers the server-side `$sample` aggregation stage (a uniform random sample,
/// cheap on large collections). `$sample` is not supported on a view, so a failing
/// aggregate falls back to a plain `find().limit(SAMPLE_SIZE)` — good enough for a
/// shape hint, and views are typically small.
async fn sample_documents(coll: &Collection<Document>) -> Result<Vec<Document>, CoreError> {
    let size = i64::try_from(SAMPLE_SIZE).unwrap_or(i64::MAX);
    let pipeline = vec![doc! { "$sample": { "size": size } }];

    match coll.aggregate(pipeline).await {
        Ok(cursor) => cursor.try_collect().await.map_err(map_mongo_err),
        // `$sample` is rejected on views (and some exotic stores); fall back to a
        // bounded find, which every readable collection/view supports. The find
        // builder's `.limit` is an i64 (the same `size` we passed to `$sample`).
        Err(_) => {
            let cursor = coll
                .find(doc! {})
                .limit(size)
                .await
                .map_err(map_mongo_err)?;
            cursor.try_collect().await.map_err(map_mongo_err)
        }
    }
}

/// Derive the [`ColumnInfo`] set from a sample of documents.
///
/// Mirrors the streaming layer's `derive_columns` (field union in first-seen
/// order, `_id` pinned first, type from the first non-null value) but produces
/// the richer introspection shape: dense 0-based `ordinal`, `is_primary_key` for
/// `_id`, and `nullable` computed from missing/null occurrences across the sample.
fn derive_columns(sample: &[Document]) -> Vec<ColumnInfo> {
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
        .map(|(i, name)| {
            // Type from the first non-null value across the sample; "mixed" when
            // the field was only ever null (or never carried a value).
            let data_type = sample
                .iter()
                .filter_map(|doc| doc.get(&name))
                .find(|v| !matches!(v, bson::Bson::Null))
                .map(|v| bson_type_name(v).to_string())
                .unwrap_or_else(|| "mixed".to_string());

            // Nullable when at least one sampled document either omits the field
            // or carries it as an explicit null.
            let nullable = sample.iter().any(|doc| match doc.get(&name) {
                None => true,
                Some(bson::Bson::Null) => true,
                Some(_) => false,
            });

            ColumnInfo {
                name: name.clone(),
                // The tree uses 0-based ordinals for MongoDB fields (there is no
                // catalog ordinal); dense from 0 in first-seen order.
                ordinal: i32::try_from(i).unwrap_or(i32::MAX),
                data_type,
                nullable,
                is_primary_key: name == "_id",
                // MongoDB fields have no declared max length.
                max_length: None,
            }
        })
        .collect()
}

/// Resolve the database to introspect: the passed `database` when non-empty,
/// else the connection's `default_db`. Errors when neither is set.
fn resolve_db<'a>(database: &'a str, default_db: Option<&'a str>) -> Result<&'a str, CoreError> {
    let db = database.trim();
    if !db.is_empty() {
        return Ok(db);
    }
    default_db
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CoreError::Query("no database selected; set one on the connection or via USE".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn columns_union_with_id_first_and_dense_ordinals() {
        let sample = vec![
            doc! { "name": "a", "qty": 1i32 },
            doc! { "_id": 1i32, "name": "b", "extra": true },
        ];
        let cols = derive_columns(&sample);
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["_id", "name", "qty", "extra"]);
        for (i, c) in cols.iter().enumerate() {
            assert_eq!(c.ordinal, i as i32);
        }
        // `_id` is flagged primary key, nothing else is.
        assert!(cols[0].is_primary_key);
        assert!(!cols[1].is_primary_key);
    }

    #[test]
    fn nullable_reflects_missing_or_null() {
        // `qty` present in one doc, missing in the other → nullable.
        // `name` present in both, always non-null → not nullable.
        // `note` explicitly null in one → nullable.
        let sample = vec![
            doc! { "name": "a", "qty": 1i32, "note": bson::Bson::Null },
            doc! { "name": "b" },
        ];
        let cols = derive_columns(&sample);
        let by = |n: &str| cols.iter().find(|c| c.name == n).unwrap();
        assert!(!by("name").nullable, "name is always present and non-null");
        assert!(by("qty").nullable, "qty is missing from one doc");
        assert!(by("note").nullable, "note is explicitly null in one doc");
    }

    #[test]
    fn data_type_inferred_from_first_non_null() {
        let sample = vec![doc! { "x": bson::Bson::Null }, doc! { "x": 42i32 }];
        let x = &derive_columns(&sample)[0];
        assert_eq!(x.data_type, "int");
        assert!(x.nullable, "a null occurrence makes it nullable");
    }

    #[test]
    fn all_null_field_is_mixed() {
        let sample = vec![doc! { "x": bson::Bson::Null }];
        let x = &derive_columns(&sample)[0];
        assert_eq!(x.data_type, "mixed");
        assert!(x.nullable);
    }

    #[test]
    fn empty_sample_has_no_columns() {
        assert!(derive_columns(&[]).is_empty());
    }

    #[test]
    fn resolve_db_prefers_arg_then_default() {
        assert_eq!(resolve_db("mydb", Some("fallback")).unwrap(), "mydb");
        assert_eq!(resolve_db("  ", Some("fallback")).unwrap(), "fallback");
        assert!(resolve_db("", None).is_err());
        assert!(resolve_db("  ", Some("   ")).is_err());
    }
}
