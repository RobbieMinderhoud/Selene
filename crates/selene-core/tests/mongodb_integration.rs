//! Integration tests for the MongoDB driver against a **real** MongoDB server.
//!
//! These spin up the official `mongo` image in Docker via [`testcontainers`]
//! (the `mongo` module of `testcontainers-modules`) and exercise `selene-core`'s
//! public MongoDB driver API:
//!
//! - **M1**: `test_connection`, `connect` + `ping`, `list_databases`.
//! - **M2**: read query execution — `find` (incl. column union + nested
//!   document/array cells + a missing-field `Null`), a filtered `find` with a
//!   `.sort().limit()` chain, `aggregate`, `countDocuments`, `distinct`,
//!   `max_rows` truncation, and a pre-cancelled token.
//! - **Writes**: `insertOne`/`insertMany`, `updateMany` (+ an `upsert` insert),
//!   `deleteOne`/`deleteMany`, `replaceOne`, and `drop` execute and report an
//!   affected-document count; a still-unsupported write (`findOneAndUpdate`) is
//!   refused as `Unsupported`.
//! - **M3**: introspection by sampling — `list_tables` (collections as `Table`,
//!   a `viewOn` view as `View`, sorted), and `list_columns` (`_id` first + flagged
//!   primary key, a sometimes-missing field reported `nullable`, types inferred).
//!   The read-only guard is a pure-logic concern covered by `mongo_guard`'s own
//!   unit tests, not here (it is enforced in `src-tauri`, not the driver).
//!
//! ## Why every test is `#[ignore]`-d
//! A plain `cargo test` must stay hermetic and fast (the unit tests need no
//! Docker), so these are gated behind `--ignored`:
//!
//! ```text
//! cargo test -p selene-core --features mongodb --test mongodb_integration -- --ignored
//! ```
//!
//! testcontainers maps the container's internal `27017` to a **random** host
//! port, so there is no conflict with any local MongoDB — the port is never
//! hardcoded. The default image runs without authentication, so the spec uses
//! `AuthMethod::None`.
//!
//! Seeding uses the `mongodb` driver's *own* client directly (writes are not part
//! of the M2 read API), inserting fictitious documents only.

#![cfg(feature = "mongodb")]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bson::{doc, oid::ObjectId, Bson, Document};
use mongodb::Client;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::mongo::Mongo;

use selene_core::driver::driver_for;
use selene_core::{
    AuthMethod, CancelToken, CellValue, Column, Connection, ConnectionSpec, CoreError, DriverId,
    ExecOptions, Flow, LogicalType, RowSink, Secret, TableKind, TlsConfig,
};

/// The database the tests seed into and query against.
const TEST_DB: &str = "selene_it";
/// The collection the tests seed into and query against.
const TEST_COLL: &str = "docs";

/// A live test fixture: the connected `Connection` plus the running container.
///
/// The `ContainerAsync` guard MUST be held for the lifetime of the test —
/// dropping it stops and removes the container (and so kills the connection).
struct Fixture {
    conn: Box<dyn Connection>,
    // Kept alive to keep the container running; never read directly.
    _container: ContainerAsync<Mongo>,
}

/// Build a `ConnectionSpec` for the mapped host port, with `TEST_DB` as the
/// default database so queries resolve their collection there.
fn spec_for(port: u16) -> ConnectionSpec {
    ConnectionSpec {
        id: "it-mongodb".to_string(),
        name: "integration".to_string(),
        driver: DriverId::Mongodb,
        host: "127.0.0.1".to_string(),
        port: Some(port),
        instance: None,
        uri: None,
        database: Some(TEST_DB.to_string()),
        auth: AuthMethod::None,
        tls: TlsConfig {
            // The dev image speaks plaintext; don't attempt a TLS handshake.
            encrypt: false,
            trust_server_certificate: false,
        },
        read_only: false,
    }
}

/// Start a fresh MongoDB container and open a `selene-core` connection to it.
async fn start_mongodb() -> (Fixture, u16) {
    let container = Mongo::default()
        .start()
        .await
        .expect("start MongoDB container");
    let port = container
        .get_host_port_ipv4(27017)
        .await
        .expect("map container port 27017 to a host port");

    let driver = driver_for(DriverId::Mongodb).expect("mongodb driver compiled in");
    let conn = driver
        .connect(&spec_for(port), &Secret::new(""))
        .await
        .expect("connect to MongoDB");

    (
        Fixture {
            conn,
            _container: container,
        },
        port,
    )
}

/// A raw `mongodb` client to the mapped port, used only for seeding fixture
/// documents (the M2 driver API is read-only).
async fn raw_client(port: u16) -> Client {
    Client::with_uri_str(format!("mongodb://127.0.0.1:{port}"))
        .await
        .expect("raw mongodb client")
}

/// Insert `docs` into `TEST_DB.TEST_COLL` via the raw client.
async fn seed(port: u16, docs: Vec<Document>) {
    let client = raw_client(port).await;
    let coll = client.database(TEST_DB).collection::<Document>(TEST_COLL);
    coll.insert_many(docs).await.expect("seed documents");
}

/// Insert `docs` into `TEST_DB.<collection>` via the raw client (introspection
/// tests seed multiple named collections).
async fn seed_named(port: u16, collection: &str, docs: Vec<Document>) {
    let client = raw_client(port).await;
    let coll = client.database(TEST_DB).collection::<Document>(collection);
    coll.insert_many(docs).await.expect("seed named collection");
}

/// Create a MongoDB **view** (`view_name` over `on`, identity pipeline) via the
/// raw client, so `list_tables` can report it as a `View`.
async fn create_view(port: u16, view_name: &str, on: &str) {
    let client = raw_client(port).await;
    client
        .database(TEST_DB)
        .create_collection(view_name)
        .view_on(on.to_string())
        // An empty pipeline makes the view mirror its source collection 1:1.
        .pipeline(Vec::<Document>::new())
        .await
        .expect("create view");
}

/// The heterogeneous fixture set: varied fields, an explicit `_id` ObjectId, a
/// nested subdocument + array, a date, an int, a double, a string, a bool, and a
/// null field. Fictitious data only.
fn fixture_docs() -> Vec<Document> {
    let oid = ObjectId::new();
    vec![
        doc! {
            "_id": oid,
            "name": "alpha",
            "qty": 3i32,
            "price": 9.99f64,
            "active": true,
            "tags": ["a", "b"],
            "meta": { "region": "eu", "tier": 1i32 },
            "created": bson::DateTime::from_millis(1_609_459_200_000), // 2021-01-01Z
            "note": Bson::Null,
        },
        doc! {
            "name": "beta",
            "qty": 7i32,
            "price": 4.50f64,
            "active": false,
            "tags": ["c"],
            "meta": { "region": "us", "tier": 2i32 },
            "created": bson::DateTime::from_millis(1_612_137_600_000), // 2021-02-01Z
            "note": "hello",
        },
    ]
}

// ---------------------------------------------------------------------------
// A recording RowSink, mirroring the sqlite/mssql harness.
// ---------------------------------------------------------------------------

/// What one result set looked like to a [`CollectingSink`].
#[derive(Clone, Debug, Default)]
struct CapturedSet {
    columns: Vec<Column>,
    rows: Vec<Vec<CellValue>>,
    set_end_count: usize,
    affected: Vec<Option<u64>>,
}

/// A [`RowSink`] recording everything per `set_index`.
#[derive(Clone, Default)]
struct CollectingSink {
    sets: Arc<Mutex<Vec<CapturedSet>>>,
}

impl CollectingSink {
    fn new() -> Self {
        Self::default()
    }

    fn sets(&self) -> Vec<CapturedSet> {
        self.sets.lock().unwrap().clone()
    }

    fn set0(&self) -> CapturedSet {
        self.sets().into_iter().next().unwrap_or_default()
    }

    fn ensure(sets: &mut Vec<CapturedSet>, set_index: usize) {
        if sets.len() <= set_index {
            sets.resize_with(set_index + 1, CapturedSet::default);
        }
    }
}

#[async_trait]
impl RowSink for CollectingSink {
    async fn on_meta(&mut self, set_index: usize, columns: Vec<Column>) -> Flow {
        let mut sets = self.sets.lock().unwrap();
        Self::ensure(&mut sets, set_index);
        sets[set_index].columns = columns;
        Flow::Continue
    }

    async fn on_rows(&mut self, set_index: usize, rows: Vec<Vec<CellValue>>) -> Flow {
        let mut sets = self.sets.lock().unwrap();
        Self::ensure(&mut sets, set_index);
        sets[set_index].rows.extend(rows);
        Flow::Continue
    }

    async fn on_set_end(&mut self, set_index: usize, affected: Option<u64>) -> Flow {
        let mut sets = self.sets.lock().unwrap();
        Self::ensure(&mut sets, set_index);
        sets[set_index].set_end_count += 1;
        sets[set_index].affected.push(affected);
        Flow::Continue
    }
}

/// Run `sql` with `opts`, collecting into a fresh sink.
async fn run(
    conn: &mut dyn Connection,
    sql: &str,
    opts: &ExecOptions,
) -> (selene_core::ExecOutcome, CollectingSink) {
    let mut sink = CollectingSink::new();
    let cancel = CancelToken::new();
    let outcome = conn
        .execute(sql, opts, &mut sink, &cancel)
        .await
        .unwrap_or_else(|e| panic!("query failed: {sql}\n  error: {e}"));
    (outcome, sink)
}

/// Find the cell for a column by name in a row.
fn cell<'a>(set: &'a CapturedSet, row: usize, col: &str) -> &'a CellValue {
    let ordinal = set
        .columns
        .iter()
        .position(|c| c.name == col)
        .unwrap_or_else(|| panic!("column {col} not found in {:?}", set.columns));
    &set.rows[row][ordinal]
}

// ---------------------------------------------------------------------------
// 1. test_connection reports a version banner (M1 regression guard)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn test_connection_reports_version() {
    let container = Mongo::default()
        .start()
        .await
        .expect("start MongoDB container");
    let port = container
        .get_host_port_ipv4(27017)
        .await
        .expect("map container port");

    let driver = driver_for(DriverId::Mongodb).unwrap();
    let report = driver
        .test_connection(&spec_for(port), &Secret::new(""))
        .await
        .expect("test_connection succeeds");

    let version = report
        .server_version
        .expect("server_version should be Some");
    assert!(
        version.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "unexpected mongodb version banner: {version}"
    );
}

// ---------------------------------------------------------------------------
// 2. connect + ping succeed (M1 regression guard)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn connect_and_ping_succeed() {
    let (mut fixture, _port) = start_mongodb().await;
    fixture.conn.ping().await.expect("ping succeeds");
}

// ---------------------------------------------------------------------------
// 3. list_databases flags system databases (M1 regression guard)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn list_databases_flags_system_databases() {
    let (mut fixture, _port) = start_mongodb().await;
    let dbs = fixture.conn.list_databases().await.expect("list_databases");

    for expected in ["admin", "local"] {
        let db = dbs
            .iter()
            .find(|d| d.name == expected)
            .unwrap_or_else(|| panic!("system database {expected} missing; got {dbs:?}"));
        assert!(db.is_system, "{expected} must be flagged is_system");
        assert_eq!(db.state_desc, "ONLINE");
    }
}

// ---------------------------------------------------------------------------
// 4. find({}) returns all rows; columns union with _id first; nested cells;
//    a missing field is Null.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn find_all_returns_rows_with_unioned_columns() {
    let (mut fixture, port) = start_mongodb().await;
    seed(port, fixture_docs()).await;

    let (outcome, sink) = run(
        fixture.conn.as_mut(),
        "db.docs.find({})",
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.result_sets, 1);
    assert_eq!(outcome.total_rows, 2);
    assert!(!outcome.truncated);

    let set = sink.set0();
    assert_eq!(set.rows.len(), 2);
    assert_eq!(set.set_end_count, 1, "the set must be closed exactly once");

    // `_id` is forced to ordinal 0.
    assert_eq!(set.columns[0].name, "_id");

    // The union of top-level fields across both documents is present.
    for expected in [
        "_id", "name", "qty", "price", "active", "tags", "meta", "created", "note",
    ] {
        assert!(
            set.columns.iter().any(|c| c.name == expected),
            "column {expected} missing from {:?}",
            set.columns
        );
    }

    // Row 0 is "alpha" (insertion order is preserved for a natural find).
    let (alpha_row, beta_row) = if matches!(cell(&set, 0, "name"), CellValue::String(s) if s == "alpha")
    {
        (0, 1)
    } else {
        (1, 0)
    };

    // Nested subdocument → a Document JSON string (relaxed extjson).
    match cell(&set, alpha_row, "meta") {
        CellValue::Document(json) => {
            assert!(json.contains("\"region\":\"eu\""), "got {json}");
            assert!(json.contains("\"tier\":1"), "got {json}");
        }
        other => panic!("meta should be a Document cell, got {other:?}"),
    }

    // Nested array → an Array JSON string.
    match cell(&set, alpha_row, "tags") {
        CellValue::Array(json) => assert_eq!(json, "[\"a\",\"b\"]"),
        other => panic!("tags should be an Array cell, got {other:?}"),
    }

    // The int/double/bool/string scalars convert to their neutral cells.
    assert_eq!(cell(&set, alpha_row, "qty"), &CellValue::I64(3));
    assert_eq!(cell(&set, alpha_row, "price"), &CellValue::F64(9.99));
    assert_eq!(cell(&set, alpha_row, "active"), &CellValue::Bool(true));

    // The date is an RFC-3339 UTC string.
    match cell(&set, alpha_row, "created") {
        CellValue::DateTime { iso, .. } => assert!(iso.starts_with("2021-01-01"), "got {iso}"),
        other => panic!("created should be a DateTime cell, got {other:?}"),
    }

    // "alpha"'s explicit `note: null` is a Null cell.
    assert_eq!(cell(&set, alpha_row, "note"), &CellValue::Null);
    // "beta" has a string note.
    assert_eq!(
        cell(&set, beta_row, "note"),
        &CellValue::String("hello".into())
    );

    // "beta" has no `_id` field of its own type set — Mongo auto-assigns one, so
    // its `_id` cell is a (hex) String, never Null.
    assert!(matches!(cell(&set, beta_row, "_id"), CellValue::String(_)));
}

// ---------------------------------------------------------------------------
// 5. find with a filter + a .sort().limit() chain.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn find_with_filter_sort_limit() {
    let (mut fixture, port) = start_mongodb().await;
    seed(port, fixture_docs()).await;

    // Only "beta" is inactive; sort by qty desc, cap at 1.
    let (outcome, sink) = run(
        fixture.conn.as_mut(),
        r#"db.docs.find({ "active": false }).sort({ "qty": -1 }).limit(1)"#,
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.total_rows, 1);
    let set = sink.set0();
    assert_eq!(set.rows.len(), 1);
    assert_eq!(cell(&set, 0, "name"), &CellValue::String("beta".into()));
    assert_eq!(cell(&set, 0, "qty"), &CellValue::I64(7));
}

// ---------------------------------------------------------------------------
// 6. aggregate([$match, $group]).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn aggregate_match_group() {
    let (mut fixture, port) = start_mongodb().await;
    seed(port, fixture_docs()).await;

    // Sum qty across all active documents (only "alpha", qty=3).
    let (outcome, sink) = run(
        fixture.conn.as_mut(),
        r#"db.docs.aggregate([{ "$match": { "active": true } }, { "$group": { "_id": "$active", "totalQty": { "$sum": "$qty" } } }])"#,
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.result_sets, 1);
    assert_eq!(outcome.total_rows, 1);
    let set = sink.set0();
    assert_eq!(cell(&set, 0, "totalQty"), &CellValue::I64(3));
}

// ---------------------------------------------------------------------------
// 7. countDocuments → single `count` row.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn count_documents_single_row() {
    let (mut fixture, port) = start_mongodb().await;
    seed(port, fixture_docs()).await;

    let (outcome, sink) = run(
        fixture.conn.as_mut(),
        "db.docs.countDocuments({})",
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.total_rows, 1);
    let set = sink.set0();
    assert_eq!(set.columns.len(), 1);
    assert_eq!(set.columns[0].name, "count");
    assert_eq!(set.columns[0].logical, LogicalType::Integer);
    assert_eq!(set.rows.len(), 1);
    assert_eq!(set.rows[0][0], CellValue::I64(2));
    assert_eq!(set.affected, vec![Some(1)]);
}

// ---------------------------------------------------------------------------
// 8. distinct → one row per distinct value.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn distinct_one_row_per_value() {
    let (mut fixture, port) = start_mongodb().await;
    // Seed three docs with two distinct regions.
    seed(
        port,
        vec![
            doc! { "region": "eu" },
            doc! { "region": "us" },
            doc! { "region": "eu" },
        ],
    )
    .await;

    let (outcome, sink) = run(
        fixture.conn.as_mut(),
        r#"db.docs.distinct("region")"#,
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.total_rows, 2, "two distinct regions");
    let set = sink.set0();
    assert_eq!(set.columns.len(), 1);
    assert_eq!(set.columns[0].name, "region");

    let mut values: Vec<String> = set
        .rows
        .iter()
        .map(|r| match &r[0] {
            CellValue::String(s) => s.clone(),
            other => panic!("expected String, got {other:?}"),
        })
        .collect();
    values.sort();
    assert_eq!(values, vec!["eu".to_string(), "us".to_string()]);
}

// ---------------------------------------------------------------------------
// 9. max_rows truncation: seed > cap docs; delivers exactly the cap; truncated.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn max_rows_truncates() {
    let (mut fixture, port) = start_mongodb().await;
    // Seed 10 trivial documents.
    let docs: Vec<Document> = (0..10).map(|i| doc! { "n": i }).collect();
    seed(port, docs).await;

    let opts = ExecOptions {
        max_rows: Some(4),
        batch_size: 2,
    };
    let (outcome, sink) = run(fixture.conn.as_mut(), "db.docs.find({})", &opts).await;

    assert_eq!(outcome.total_rows, 4, "delivers exactly the cap");
    assert!(outcome.truncated, "the source had more than the cap");
    assert_eq!(sink.set0().rows.len(), 4);
}

// ---------------------------------------------------------------------------
// 10. A pre-cancelled token → Cancelled.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn precancelled_token_yields_cancelled() {
    let (mut fixture, port) = start_mongodb().await;
    seed(port, fixture_docs()).await;

    let mut sink = CollectingSink::new();
    let cancel = CancelToken::new();
    cancel.cancel();

    let err = fixture
        .conn
        .execute(
            "db.docs.find({})",
            &ExecOptions::default(),
            &mut sink,
            &cancel,
        )
        .await
        .expect_err("a pre-cancelled token must abort the query");
    assert!(matches!(err, CoreError::Cancelled), "got {err:?}");
}

// ---------------------------------------------------------------------------
// 11. Writes execute and report an affected-document count; a still-unsupported
//     write is refused as Unsupported.
// ---------------------------------------------------------------------------

/// Count documents in `TEST_DB.<collection>` via the raw client (used to verify
/// a write's effect independently of the driver's read path).
async fn raw_count(port: u16, collection: &str) -> u64 {
    let client = raw_client(port).await;
    client
        .database(TEST_DB)
        .collection::<Document>(collection)
        .count_documents(doc! {})
        .await
        .expect("raw count")
}

/// Whether `TEST_DB` currently contains a collection named `collection`.
async fn raw_collection_exists(port: u16, collection: &str) -> bool {
    let client = raw_client(port).await;
    let names = client
        .database(TEST_DB)
        .list_collection_names()
        .await
        .expect("list collection names");
    names.iter().any(|n| n == collection)
}

/// The single affected count of a write's column-less result set.
fn affected(sink: &CollectingSink) -> Option<u64> {
    let set = sink.set0();
    // A write emits meta with no columns, then one set-end carrying the count.
    assert!(
        set.columns.is_empty(),
        "a write result set must be column-less, got {:?}",
        set.columns
    );
    assert_eq!(set.set_end_count, 1, "the set must be closed exactly once");
    set.affected.into_iter().next().flatten()
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn insert_one_executes() {
    // Verified via the driver's own read path, so no raw client / port needed.
    let (mut fixture, _port) = start_mongodb().await;

    let (outcome, sink) = run(
        fixture.conn.as_mut(),
        r#"db.docs.insertOne({ "name": "gamma", "qty": 5 })"#,
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.result_sets, 1);
    assert_eq!(affected(&sink), Some(1));
    // The document is now findable via the driver's own read path.
    let (count_outcome, count_sink) = run(
        fixture.conn.as_mut(),
        r#"db.docs.countDocuments({ "name": "gamma" })"#,
        &ExecOptions::default(),
    )
    .await;
    assert_eq!(count_outcome.total_rows, 1);
    assert_eq!(count_sink.set0().rows[0][0], CellValue::I64(1));
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn insert_many_reports_count() {
    let (mut fixture, port) = start_mongodb().await;

    let (_outcome, sink) = run(
        fixture.conn.as_mut(),
        r#"db.docs.insertMany([{ "n": 1 }, { "n": 2 }, { "n": 3 }])"#,
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(affected(&sink), Some(3));
    assert_eq!(raw_count(port, TEST_COLL).await, 3);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn update_many_modifies_matching_documents() {
    let (mut fixture, port) = start_mongodb().await;
    seed(port, fixture_docs()).await; // "alpha" (active:true), "beta" (active:false)

    // Set a new field on every document; both match, both are modified.
    let (_outcome, sink) = run(
        fixture.conn.as_mut(),
        r#"db.docs.updateMany({}, { "$set": { "reviewed": true } })"#,
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(affected(&sink), Some(2), "both documents modified");
    // Verify the change landed on both.
    let (count_outcome, _) = run(
        fixture.conn.as_mut(),
        r#"db.docs.countDocuments({ "reviewed": true })"#,
        &ExecOptions::default(),
    )
    .await;
    assert_eq!(count_outcome.total_rows, 1);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn delete_one_and_delete_many_remove_documents() {
    let (mut fixture, port) = start_mongodb().await;
    // Four docs: two "eu", two "us".
    seed(
        port,
        vec![
            doc! { "region": "eu", "n": 1i32 },
            doc! { "region": "eu", "n": 2i32 },
            doc! { "region": "us", "n": 3i32 },
            doc! { "region": "us", "n": 4i32 },
        ],
    )
    .await;

    // deleteOne removes exactly one matching "eu" document.
    let (_o1, s1) = run(
        fixture.conn.as_mut(),
        r#"db.docs.deleteOne({ "region": "eu" })"#,
        &ExecOptions::default(),
    )
    .await;
    assert_eq!(affected(&s1), Some(1));
    assert_eq!(raw_count(port, TEST_COLL).await, 3);

    // deleteMany removes both "us" documents.
    let (_o2, s2) = run(
        fixture.conn.as_mut(),
        r#"db.docs.deleteMany({ "region": "us" })"#,
        &ExecOptions::default(),
    )
    .await;
    assert_eq!(affected(&s2), Some(2));
    assert_eq!(raw_count(port, TEST_COLL).await, 1);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn replace_one_swaps_the_document() {
    let (mut fixture, port) = start_mongodb().await;
    seed(port, vec![doc! { "name": "old", "keep": false }]).await;

    let (_outcome, sink) = run(
        fixture.conn.as_mut(),
        r#"db.docs.replaceOne({ "name": "old" }, { "name": "new" })"#,
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(affected(&sink), Some(1), "one document replaced");
    // The old shape is gone; the replacement is present without the old field.
    let (gone, _) = run(
        fixture.conn.as_mut(),
        r#"db.docs.countDocuments({ "name": "old" })"#,
        &ExecOptions::default(),
    )
    .await;
    assert_eq!(gone.total_rows, 1);
    assert_eq!(
        run(
            fixture.conn.as_mut(),
            r#"db.docs.countDocuments({ "name": "new" })"#,
            &ExecOptions::default(),
        )
        .await
        .0
        .total_rows,
        1
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn upsert_inserts_when_no_match() {
    let (mut fixture, port) = start_mongodb().await;
    seed(port, vec![doc! { "name": "existing" }]).await;

    // A filter that matches nothing, with upsert:true, inserts a new document —
    // counted as one affected even though modified_count is 0.
    let (_outcome, sink) = run(
        fixture.conn.as_mut(),
        r#"db.docs.updateOne({ "name": "missing" }, { "$set": { "created": true } }, { "upsert": true })"#,
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(affected(&sink), Some(1), "an upsert insert counts as one");
    assert_eq!(raw_count(port, TEST_COLL).await, 2);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn drop_removes_the_collection() {
    let (mut fixture, port) = start_mongodb().await;
    seed_named(port, "scratch", vec![doc! { "n": 1i32 }]).await;
    assert!(raw_collection_exists(port, "scratch").await);

    // Point the connection's current collection queries at "scratch" via a
    // fully-qualified drop.
    let (outcome, sink) = run(
        fixture.conn.as_mut(),
        "db.scratch.drop()",
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.result_sets, 1);
    // A drop returns no count; we surface a clean 0.
    assert_eq!(affected(&sink), Some(0));
    assert!(
        !raw_collection_exists(port, "scratch").await,
        "the collection must be gone after drop()"
    );
    // It also disappears from the driver's own list_tables.
    let tables = fixture
        .conn
        .list_tables(TEST_DB, "")
        .await
        .expect("list_tables");
    assert!(
        !tables.iter().any(|t| t.name == "scratch"),
        "dropped collection must not appear in list_tables"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn still_unsupported_write_is_refused() {
    let (mut fixture, _port) = start_mongodb().await;
    let mut sink = CollectingSink::new();
    let cancel = CancelToken::new();
    // findOneAndUpdate returns a document (a later change); it stays Unsupported.
    let err = fixture
        .conn
        .execute(
            r#"db.docs.findOneAndUpdate({}, { "$set": { "a": 1 } })"#,
            &ExecOptions::default(),
            &mut sink,
            &cancel,
        )
        .await
        .expect_err("findOneAndUpdate is not yet supported");
    assert!(matches!(err, CoreError::Unsupported(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// 12. An empty result (no matching docs) yields an empty grid, not an error.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn empty_result_is_empty_grid() {
    let (mut fixture, port) = start_mongodb().await;
    seed(port, fixture_docs()).await;

    let (outcome, sink) = run(
        fixture.conn.as_mut(),
        r#"db.docs.find({ "name": "nonexistent" })"#,
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.total_rows, 0);
    assert!(!outcome.truncated);
    let set = sink.set0();
    assert!(set.rows.is_empty());
    assert_eq!(set.set_end_count, 1, "an empty set is still closed once");
    assert_eq!(set.affected, vec![Some(0)]);
}

// ---------------------------------------------------------------------------
// 13. (M3) list_tables reports collections as Table and a view as View, sorted.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn list_tables_reports_collections_and_views() {
    let (mut fixture, port) = start_mongodb().await;
    // Two base collections plus a view over one of them.
    seed_named(port, "orders", vec![doc! { "n": 1i32 }]).await;
    seed_named(port, "customers", vec![doc! { "n": 2i32 }]).await;
    create_view(port, "orders_view", "orders").await;

    let tables = fixture
        .conn
        .list_tables(TEST_DB, "")
        .await
        .expect("list_tables");

    // Sorted by name for a stable tree.
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "tables must be name-sorted, got {names:?}");

    // MongoDB has no schema level.
    assert!(tables.iter().all(|t| t.schema.is_empty()));

    let kind = |name: &str| {
        tables
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} missing from {names:?}"))
            .kind
    };
    assert_eq!(kind("orders"), TableKind::Table);
    assert_eq!(kind("customers"), TableKind::Table);
    assert_eq!(
        kind("orders_view"),
        TableKind::View,
        "the view must be View"
    );
}

// ---------------------------------------------------------------------------
// 14. (M3) list_columns samples fields: _id first + primary key, a
//     sometimes-missing field is nullable, types inferred.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn list_columns_samples_field_shape() {
    let (mut fixture, port) = start_mongodb().await;
    // Heterogeneous docs: an explicit _id, nested fields, and `note` present in
    // only one document (so it must be reported nullable).
    seed(
        port,
        vec![
            doc! {
                "_id": ObjectId::new(),
                "name": "alpha",
                "qty": 3i32,
                "meta": { "region": "eu" },
                "note": "present",
            },
            doc! {
                "_id": ObjectId::new(),
                "name": "beta",
                "qty": 7i32,
                "meta": { "region": "us" },
                // no `note` field → `note` must be nullable
            },
        ],
    )
    .await;

    let cols = fixture
        .conn
        .list_columns(TEST_DB, "", TEST_COLL)
        .await
        .expect("list_columns");

    // `_id` is forced first, at ordinal 0, and flagged primary key.
    assert_eq!(cols[0].name, "_id", "columns: {cols:?}");
    assert_eq!(cols[0].ordinal, 0);
    assert!(cols[0].is_primary_key, "_id must be the primary key");
    assert!(cols.iter().skip(1).all(|c| !c.is_primary_key));

    let by = |name: &str| {
        cols.iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("column {name} missing from {cols:?}"))
    };

    // Types inferred from the sampled values.
    assert_eq!(by("_id").data_type, "objectId");
    assert_eq!(by("name").data_type, "string");
    assert_eq!(by("qty").data_type, "int");
    assert_eq!(by("meta").data_type, "object");

    // A field present in every document with a value is not nullable; `note`
    // (missing from one document) is nullable.
    assert!(!by("name").nullable, "name is always present");
    assert!(by("note").nullable, "note is missing from one document");

    // Ordinals are dense and 0-based.
    for (i, c) in cols.iter().enumerate() {
        assert_eq!(c.ordinal, i as i32, "ordinals must be dense: {cols:?}");
    }
}
