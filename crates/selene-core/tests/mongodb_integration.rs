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
//!   `max_rows` truncation, a pre-cancelled token, and that a write method is
//!   refused as `Unsupported`.
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
    ExecOptions, Flow, LogicalType, RowSink, Secret, TlsConfig,
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
// 11. A write method is refused as Unsupported.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn write_method_is_unsupported() {
    let (mut fixture, _port) = start_mongodb().await;
    let mut sink = CollectingSink::new();
    let cancel = CancelToken::new();
    let err = fixture
        .conn
        .execute(
            r#"db.docs.insertOne({ "a": 1 })"#,
            &ExecOptions::default(),
            &mut sink,
            &cancel,
        )
        .await
        .expect_err("writes are not supported in M2");
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
