//! MongoDB driver, backed by the official [`mongodb`] crate over Tokio.
//!
//! MongoDB is a document store, not a SQL engine, so it does not fit Selene's
//! relational model cleanly. This driver threads it through the existing
//! [`Connection`]/[`DatabaseDriver`] traits anyway; support arrives in stages:
//!
//! - **M1**: connect / ping / `test_connection` / capabilities +
//!   `list_databases`.
//! - **M2**: read query execution — a mongosh-shell-subset parser ([`query`]),
//!   BSON→[`CellValue`] conversion ([`convert`]), and streaming of
//!   `find`/`aggregate`/`countDocuments`/`distinct` into the result grid
//!   ([`stream`]).
//! - **M3**: introspection by sampling ([`introspect`]) — collections as tables
//!   (views detected via `listCollections`), fields inferred as columns from a
//!   document sample. The read-only *guard* lives in `crate::guard::mongo_guard`
//!   (enforced server-side in `src-tauri`), not in the driver.
//! - **Writes**: the core write methods ([`writes`]) — `insertOne`/`insertMany`,
//!   `updateOne`/`updateMany`, `deleteOne`/`deleteMany`, `replaceOne`, and
//!   collection `drop` — execute, reporting an affected-document count. The guard
//!   still Confirms these when writable and Blocks them read-only. Higher-level
//!   writes (`findOneAnd…`, `bulkWrite`, index/database DDL) stay `Unsupported`.
//!
//! Layout:
//! - [`config`]     — `ConnectionSpec` + `Secret` → [`mongodb::options::ClientOptions`].
//! - [`error`]      — `mongodb::error::Error` → [`CoreError`] (never leaking secrets).
//! - [`query`]      — mongosh-subset parser → [`query::MongoQuery`].
//! - [`convert`]    — BSON → [`CellValue`] / [`LogicalType`].
//! - [`stream`]     — cursor/count/distinct → [`RowSink`] events.
//! - [`writes`]     — insert/update/delete/replace/drop → affected-count set.
//! - [`introspect`] — `listCollections` + sampled-field columns.
//!
//! TLS uses the **rustls** backend (never native-tls — it breaks the handshake
//! on macOS, exactly as for the mssql/sqlx drivers).

mod config;
mod convert;
mod error;
mod introspect;
mod query;
mod stream;
mod writes;

use std::time::Instant;

use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;
use mongodb::{Client, Collection};

use crate::capabilities::DriverCapabilities;
use crate::connection_spec::{ConnectionSpec, DriverId};
use crate::driver::{
    CancelToken, Connection, DatabaseDriver, ExecOptions, ExecOutcome, RowSink, TestReport,
};
use crate::error::CoreError;
use crate::introspect::{ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo};
use crate::secret::Secret;

use self::config::build_options;
use self::error::{map_connect_err, map_mongo_err};
use self::query::{parse as parse_query, MongoQuery};

/// Databases MongoDB treats as internal/system.
const SYSTEM_DATABASES: &[&str] = &["admin", "local", "config"];

/// The MongoDB backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct MongodbDriver;

impl MongodbDriver {
    pub fn new() -> Self {
        Self
    }

    /// Capabilities of the MongoDB backend. It has no SQL schema level and no
    /// server-side cancel of a running find, but it does have multi-database
    /// servers, transactions, and (eventually) streamed cursors.
    pub const fn caps() -> DriverCapabilities {
        DriverCapabilities {
            schemas: false,
            multiple_result_sets: false,
            server_side_cancel: false,
            transactions: true,
            explain_plan: false,
            streaming_rows: true,
            list_databases: true,
            data_editing: false,
            backup_restore: false,
            database_create_drop: false,
            database_rename: false,
            database_online_offline: false,
        }
    }
}

/// Build a [`Client`] from a spec + secret and verify liveness with a `{ping:1}`
/// on the `admin` database. The `mongodb::Client` is lazily-connected and
/// internally pooled, so the ping is what actually surfaces a connect failure.
async fn open_client(spec: &ConnectionSpec, secret: &Secret) -> Result<Client, CoreError> {
    let options = build_options(spec, secret).await?;
    // NB: `options` holds the exposed password — never log or Debug it.
    let client = Client::with_options(options).map_err(map_connect_err)?;
    client
        .database("admin")
        .run_command(doc! { "ping": 1 })
        .await
        .map_err(map_connect_err)?;
    Ok(client)
}

#[async_trait::async_trait]
impl DatabaseDriver for MongodbDriver {
    fn id(&self) -> DriverId {
        DriverId::Mongodb
    }

    fn capabilities(&self) -> DriverCapabilities {
        Self::caps()
    }

    async fn test_connection(
        &self,
        spec: &ConnectionSpec,
        secret: &Secret,
    ) -> Result<TestReport, CoreError> {
        let started = Instant::now();
        let options = build_options(spec, secret).await?;
        let client = Client::with_options(options).map_err(map_connect_err)?;

        // `buildInfo` returns the server version banner and doubles as a liveness
        // check (it forces server selection + a round-trip).
        let server_version = {
            let info = client
                .database("admin")
                .run_command(doc! { "buildInfo": 1 })
                .await
                .map_err(map_connect_err)?;
            info.get_str("version").ok().map(str::to_string)
        };

        let elapsed_ms = started.elapsed().as_millis() as u64;

        // The client has no explicit close; dropping it tears down the pool.
        Ok(TestReport {
            server_version,
            elapsed_ms,
        })
    }

    async fn connect(
        &self,
        spec: &ConnectionSpec,
        secret: &Secret,
    ) -> Result<Box<dyn Connection>, CoreError> {
        let client = open_client(spec, secret).await?;
        // Track the spec's default database so later (M2) queries can default
        // their collection's database context.
        let default_db = spec
            .database
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Ok(Box::new(MongodbConnection { client, default_db }))
    }
}

/// A live MongoDB connection. Owns one [`Client`] (which is `Clone` and
/// internally connection-pooled). `default_db` tracks the "current database" the
/// way [`Connection::use_database`] expects — MongoDB has no session-bound
/// current-database concept, so we track it ourselves.
struct MongodbConnection {
    client: Client,
    default_db: Option<String>,
}

impl MongodbConnection {
    /// Resolve the current database and return a typed handle to `collection`
    /// within it. The collection lives in the connection's current database
    /// (tracked in `default_db`, set at connect time or via `use_database`);
    /// MongoDB has no session-bound current database of its own.
    fn collection(&self, collection: &str) -> Result<Collection<Document>, CoreError> {
        let db = self
            .default_db
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CoreError::Query(
                    "no database selected; set one on the connection or via USE".into(),
                )
            })?;
        Ok(self.client.database(db).collection::<Document>(collection))
    }
}

/// Convert a parsed [`Bson`] that must be a document into an owned [`Document`].
/// The parser guarantees filter/projection/sort/count args are documents, but a
/// pipeline stage comes straight from user JSON, so a non-document surfaces as a
/// [`CoreError::Query`] rather than an `expect`/panic.
fn into_document(value: mongodb::bson::Bson) -> Result<Document, CoreError> {
    match value {
        mongodb::bson::Bson::Document(d) => Ok(d),
        _ => Err(CoreError::Query(
            "expected a document object `{ … }`".into(),
        )),
    }
}

/// Like [`into_document`] but for an optional argument (projection/sort).
fn optional_document(value: Option<mongodb::bson::Bson>) -> Result<Option<Document>, CoreError> {
    match value {
        None => Ok(None),
        Some(v) => Ok(Some(into_document(v)?)),
    }
}

/// The effective server-side `find` limit backstop, given the user's explicit
/// `.limit(n)` and the `max_rows` row cap.
///
/// Two distinct notions of "limit" interact:
/// - A user's `.limit(n)` is a **real cap** — the user asked for exactly `n`
///   rows, so returning `n` is a complete result, *not* a truncation.
/// - `max_rows` is a **safety cap** — the streaming layer must be able to tell
///   whether the source had *more* than the cap so it can flag `truncated`. To
///   do that it needs to *see* one row past the cap, so the server backstop for
///   the row cap is `max_rows + 1` (fetch one extra; only `max_rows` are ever
///   delivered — the stream drops the surplus and sets `truncated`).
///
/// Combined: take the tighter of the (magnitude of the) user limit and the
/// `max_rows + 1` backstop. If the user limit is the tighter one it wins exactly
/// (no truncation); if the row cap is tighter, the `+1` lets truncation be
/// detected. A **negative** user limit is a mongosh single-batch hint; we use its
/// magnitude. With neither set the find is unbounded server-side and the
/// streaming layer's `max_rows` is the only cap.
fn server_limit(user_limit: Option<i64>, max_rows: Option<u64>) -> Option<i64> {
    let user = user_limit.map(|n| n.unsigned_abs());
    // Fetch one past the row cap so the stream can observe "there was more".
    let cap_backstop = max_rows.map(|c| c.saturating_add(1));
    let effective = match (user, cap_backstop) {
        (Some(u), Some(c)) => Some(u.min(c)),
        (Some(u), None) => Some(u),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    };
    // Clamp to i64 (FindOptions::limit is i64); a cap above i64::MAX is not
    // physically reachable.
    effective.map(|n| i64::try_from(n).unwrap_or(i64::MAX))
}

#[async_trait::async_trait]
impl Connection for MongodbConnection {
    async fn execute(
        &mut self,
        sql: &str,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
        cancel: &CancelToken,
    ) -> Result<ExecOutcome, CoreError> {
        // Honour a token that fired before we did any work.
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }

        // Parse the mongosh-subset query. A malformed query surfaces here as
        // `Query`; a not-yet-supported method (findOneAnd…, bulkWrite, DDL) as
        // `Unsupported`.
        let query = parse_query(sql)?;

        match query {
            MongoQuery::Find {
                collection,
                filter,
                projection,
                sort,
                skip,
                limit,
            } => {
                let coll = self.collection(&collection)?;
                let filter_doc = into_document(filter)?;

                // Build FindOptions from the parsed chain. `max_rows` is also
                // pushed to the server as a `.limit()` backstop so we never
                // over-read: when the user set an explicit limit we take the
                // tighter of the two; otherwise the row cap alone applies.
                let mut find_opts = FindOptions::default();
                find_opts.projection = optional_document(projection)?;
                find_opts.sort = optional_document(sort)?;
                find_opts.skip = skip;
                // Batch the cursor at the caller's batch size (bounded to a u32).
                find_opts.batch_size =
                    Some(u32::try_from(opts.batch_size.max(1)).unwrap_or(u32::MAX));
                find_opts.limit = server_limit(limit, opts.max_rows);

                let cursor = coll
                    .find(filter_doc)
                    .with_options(find_opts)
                    .await
                    .map_err(map_mongo_err)?;
                stream::stream_cursor(cursor, opts, sink, cancel).await
            }

            MongoQuery::Aggregate {
                collection,
                pipeline,
            } => {
                let coll = self.collection(&collection)?;
                // Each stage must be a document; convert, surfacing a Query error
                // for a non-document stage (e.g. `aggregate([1])`).
                let stages: Vec<Document> = pipeline
                    .into_iter()
                    .map(into_document)
                    .collect::<Result<_, _>>()?;
                let cursor = coll.aggregate(stages).await.map_err(map_mongo_err)?;
                stream::stream_cursor(cursor, opts, sink, cancel).await
            }

            MongoQuery::CountDocuments { collection, filter } => {
                let coll = self.collection(&collection)?;
                let filter_doc = into_document(filter)?;
                let count = coll
                    .count_documents(filter_doc)
                    .await
                    .map_err(map_mongo_err)?;
                stream::emit_count(count, sink).await
            }

            MongoQuery::Distinct {
                collection,
                field,
                filter,
            } => {
                let coll = self.collection(&collection)?;
                let filter_doc = into_document(filter)?;
                let values = coll
                    .distinct(&field, filter_doc)
                    .await
                    .map_err(map_mongo_err)?;
                stream::emit_distinct(&field, values, opts, sink, cancel).await
            }

            // --- Writes. The guard (server-side) refuses these on a read-only
            // connection and Confirms them on a writable one before `execute`
            // runs, so we only ever reach here for an approved write. Each emits
            // a column-less affected-count set (see `writes`).
            MongoQuery::InsertOne {
                collection,
                document,
            } => {
                let coll = self.collection(&collection)?;
                writes::insert_one(coll, into_document(document)?, sink, cancel).await
            }

            MongoQuery::InsertMany {
                collection,
                documents,
            } => {
                let coll = self.collection(&collection)?;
                let docs: Vec<Document> = documents
                    .into_iter()
                    .map(into_document)
                    .collect::<Result<_, _>>()?;
                writes::insert_many(coll, docs, sink, cancel).await
            }

            MongoQuery::UpdateOne {
                collection,
                filter,
                update,
                upsert,
            } => {
                let coll = self.collection(&collection)?;
                writes::update_one(
                    coll,
                    into_document(filter)?,
                    into_document(update)?,
                    upsert,
                    sink,
                    cancel,
                )
                .await
            }

            MongoQuery::UpdateMany {
                collection,
                filter,
                update,
                upsert,
            } => {
                let coll = self.collection(&collection)?;
                writes::update_many(
                    coll,
                    into_document(filter)?,
                    into_document(update)?,
                    upsert,
                    sink,
                    cancel,
                )
                .await
            }

            MongoQuery::ReplaceOne {
                collection,
                filter,
                replacement,
                upsert,
            } => {
                let coll = self.collection(&collection)?;
                writes::replace_one(
                    coll,
                    into_document(filter)?,
                    into_document(replacement)?,
                    upsert,
                    sink,
                    cancel,
                )
                .await
            }

            MongoQuery::DeleteOne { collection, filter } => {
                let coll = self.collection(&collection)?;
                writes::delete_one(coll, into_document(filter)?, sink, cancel).await
            }

            MongoQuery::DeleteMany { collection, filter } => {
                let coll = self.collection(&collection)?;
                writes::delete_many(coll, into_document(filter)?, sink, cancel).await
            }

            MongoQuery::DropCollection { collection } => {
                let coll = self.collection(&collection)?;
                writes::drop_collection(coll, sink, cancel).await
            }
        }
    }

    async fn list_databases(&mut self) -> Result<Vec<DatabaseInfo>, CoreError> {
        let names = self
            .client
            .list_database_names()
            .await
            .map_err(map_mongo_err)?;
        Ok(names
            .into_iter()
            .map(|name| {
                let is_system = SYSTEM_DATABASES.contains(&name.as_str());
                DatabaseInfo {
                    name,
                    is_system,
                    // MongoDB has no offline/online database state.
                    state_desc: "ONLINE".to_string(),
                }
            })
            .collect())
    }

    async fn list_schemas(&mut self, _database: &str) -> Result<Vec<SchemaInfo>, CoreError> {
        // MongoDB has no schema level (capabilities advertise `schemas: false`).
        Ok(Vec::new())
    }

    async fn list_tables(
        &mut self,
        database: &str,
        _schema: &str,
    ) -> Result<Vec<TableInfo>, CoreError> {
        // MongoDB has no schema level; collections are the "tables". See
        // `introspect::list_tables` (views detected via listCollections' type).
        introspect::list_tables(&self.client, self.default_db.as_deref(), database).await
    }

    async fn list_columns(
        &mut self,
        database: &str,
        _schema: &str,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, CoreError> {
        // Documents are schemaless, so columns are inferred by sampling. See
        // `introspect::list_columns` (`$sample`, falling back to a bounded find).
        introspect::list_columns(&self.client, self.default_db.as_deref(), database, table).await
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        self.client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .await
            .map_err(map_mongo_err)?;
        Ok(())
    }

    async fn current_database(&mut self) -> Result<String, CoreError> {
        Ok(self.default_db.clone().unwrap_or_default())
    }

    async fn use_database(&mut self, database: &str) -> Result<(), CoreError> {
        // MongoDB has no session-bound current database; tracking it lets later
        // queries default their collection's database.
        self.default_db = Some(database.to_string());
        Ok(())
    }

    // All remaining optional methods (create_table/import_rows/backup/restore +
    // database admin) use the trait defaults (Unsupported).
}

#[cfg(test)]
mod tests {
    use super::server_limit;

    #[test]
    fn server_limit_adds_one_to_the_row_cap_for_truncation_detection() {
        // Only a row cap: fetch cap+1 so the stream can see there was more.
        assert_eq!(server_limit(None, Some(4)), Some(5));
        // No caps at all: unbounded server-side.
        assert_eq!(server_limit(None, None), None);
    }

    #[test]
    fn server_limit_user_limit_wins_when_tighter_and_is_exact() {
        // A user .limit() below the row cap is a complete result (no +1).
        assert_eq!(server_limit(Some(3), Some(50_000)), Some(3));
        assert_eq!(server_limit(Some(10), None), Some(10));
    }

    #[test]
    fn server_limit_row_cap_wins_when_user_limit_exceeds_it() {
        // User asked for 100 but the safety cap is 4 → fetch cap+1 (5) to detect
        // that the (100-wide) result was truncated to 4.
        assert_eq!(server_limit(Some(100), Some(4)), Some(5));
    }

    #[test]
    fn server_limit_normalises_negative_user_limit_to_its_magnitude() {
        // A negative mongosh limit is a single-batch hint; use |n|.
        assert_eq!(server_limit(Some(-3), Some(50_000)), Some(3));
    }
}
