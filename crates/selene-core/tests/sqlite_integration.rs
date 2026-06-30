//! Integration tests for the SQLite driver against a **real** SQLite database.
//!
//! Unlike the mssql tests, these need no Docker: SQLite is in-process, so they
//! run on a plain `cargo test -p selene-core --features sqlite` and are **not**
//! `#[ignore]`-d. Each test uses a fresh temp `.db` file.
//!
//! The Selene driver opens with `create_if_missing(false)` (a missing file is an
//! error, never a silent create), so the harness pre-creates a zero-byte file —
//! which SQLite initialises as an empty database on first write — and then runs
//! all schema/data setup through the driver's own `execute` path.

#![cfg(feature = "sqlite")]

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use selene_core::driver::driver_for;
use selene_core::{
    AuthMethod, CancelToken, CellValue, Column, Connection, CoreError, DriverId, ExecOptions, Flow,
    ImportTarget, RowSink, RowSource, Secret, TableKind, TlsConfig,
};
use selene_core::{ConnectionSpec, LogicalType};

// ---------------------------------------------------------------------------
// Fixture + harness
// ---------------------------------------------------------------------------

/// A live test fixture: the connected `Connection` plus the temp file guard.
///
/// The `NamedTempFile` MUST be held for the test's lifetime — dropping it
/// deletes the database file.
struct Fixture {
    conn: Box<dyn Connection>,
    _file: tempfile::NamedTempFile,
}

/// Create a fresh, empty `.db` file and open a Selene SQLite connection to it.
async fn start_sqlite() -> Fixture {
    // A zero-byte file is a valid (empty) SQLite database; create it so the
    // driver's `create_if_missing(false)` open succeeds.
    let file = tempfile::Builder::new()
        .suffix(".db")
        .tempfile()
        .expect("create temp .db file");
    let path = file.path().to_string_lossy().to_string();

    let spec = ConnectionSpec {
        id: "it-sqlite".to_string(),
        name: "integration".to_string(),
        driver: DriverId::Sqlite,
        host: path,
        port: None,
        instance: None,
        database: None,
        auth: AuthMethod::None,
        tls: TlsConfig::default(),
        read_only: false,
    };

    let driver = driver_for(DriverId::Sqlite).expect("sqlite driver compiled in");
    let conn = driver
        .connect(&spec, &Secret::new(""))
        .await
        .expect("connect to SQLite");

    Fixture { conn, _file: file }
}

/// Build a spec pointing at `path` for `test_connection` (no live `connect`).
fn spec_for(path: &str) -> ConnectionSpec {
    ConnectionSpec {
        id: "it-sqlite".to_string(),
        name: "integration".to_string(),
        driver: DriverId::Sqlite,
        host: path.to_string(),
        port: None,
        instance: None,
        database: None,
        auth: AuthMethod::None,
        tls: TlsConfig::default(),
        read_only: false,
    }
}

/// What one result set looked like to a [`CollectingSink`].
#[derive(Clone, Debug, Default)]
struct CapturedSet {
    columns: Vec<Column>,
    rows: Vec<Vec<CellValue>>,
    batch_count: usize,
    set_end_count: usize,
    affected: Vec<Option<u64>>,
}

/// A [`RowSink`] recording everything per `set_index` — mirrors the mssql test's
/// sink so the assertions read the same way.
#[derive(Clone, Default)]
struct CollectingSink {
    sets: Arc<Mutex<Vec<CapturedSet>>>,
    cancel_after_batches: Option<usize>,
    cancel_token: Option<CancelToken>,
    batches_seen: Arc<Mutex<usize>>,
}

impl CollectingSink {
    fn new() -> Self {
        Self::default()
    }

    fn sets(&self) -> Vec<CapturedSet> {
        self.sets.lock().unwrap().clone()
    }

    fn total_rows(&self) -> usize {
        self.sets.lock().unwrap().iter().map(|s| s.rows.len()).sum()
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
        {
            let mut sets = self.sets.lock().unwrap();
            Self::ensure(&mut sets, set_index);
            let set = &mut sets[set_index];
            set.batch_count += 1;
            set.rows.extend(rows);
        }
        let seen = {
            let mut b = self.batches_seen.lock().unwrap();
            *b += 1;
            *b
        };
        // Cooperative-cancellation hook (unused by these tests but kept for
        // parity with the mssql harness): trip the token after the Nth batch.
        if let (Some(after), Some(token)) = (self.cancel_after_batches, &self.cancel_token) {
            if seen >= after {
                token.cancel();
            }
        }
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
        .expect("query executes");
    (outcome, sink)
}

/// Execute a statement for its side effects, panicking on error.
async fn exec_ok(conn: &mut dyn Connection, sql: &str) {
    let mut sink = CollectingSink::new();
    let cancel = CancelToken::new();
    conn.execute(sql, &ExecOptions::default(), &mut sink, &cancel)
        .await
        .unwrap_or_else(|e| panic!("statement failed: {sql}\n  error: {e}"));
}

/// A trivial in-memory [`RowSource`] for the import tests: yields its rows once,
/// then signals exhaustion with an empty batch.
struct VecRowSource {
    rows: Vec<Vec<CellValue>>,
    done: bool,
}

impl VecRowSource {
    fn new(rows: Vec<Vec<CellValue>>) -> Self {
        Self { rows, done: false }
    }
}

#[async_trait]
impl RowSource for VecRowSource {
    async fn next_batch(&mut self) -> Result<Vec<Vec<CellValue>>, CoreError> {
        if self.done {
            return Ok(Vec::new());
        }
        self.done = true;
        Ok(std::mem::take(&mut self.rows))
    }
}

// ---------------------------------------------------------------------------
// 1. connect + test_connection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connect_and_test_connection_reports_version() {
    let file = tempfile::Builder::new()
        .suffix(".db")
        .tempfile()
        .expect("temp .db");
    let path = file.path().to_string_lossy().to_string();

    let driver = driver_for(DriverId::Sqlite).unwrap();
    let report = driver
        .test_connection(&spec_for(&path), &Secret::new(""))
        .await
        .expect("test_connection succeeds");

    let version = report
        .server_version
        .expect("server_version should be Some");
    // sqlite_version() looks like "3.45.0" — a dotted numeric string.
    assert!(
        version.split('.').next().is_some_and(|m| m == "3"),
        "unexpected sqlite version banner: {version}"
    );
}

#[tokio::test]
async fn connecting_to_a_missing_file_errors() {
    // create_if_missing(false): a path that does not exist must fail, not create.
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("does_not_exist.db");
    let driver = driver_for(DriverId::Sqlite).unwrap();
    // `Box<dyn Connection>` is not `Debug`, so match rather than `expect_err`.
    match driver
        .connect(&spec_for(&missing.to_string_lossy()), &Secret::new(""))
        .await
    {
        // Surfaced as a connection (or config) failure, never a silent success.
        Err(CoreError::Connection(_)) | Err(CoreError::Config(_)) => {}
        Err(other) => panic!("expected Connection/Config, got {other:?}"),
        Ok(_) => panic!("connecting to a missing file must error, not succeed"),
    }
}

// ---------------------------------------------------------------------------
// 2. typed scalar SELECT (storage-class mapping)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn typed_scalar_select_maps_every_storage_class() {
    let mut fixture = start_sqlite().await;
    let conn = fixture.conn.as_mut();

    // A table with one column per storage class plus a NULL.
    exec_ok(
        conn,
        "CREATE TABLE t ( \
            i INTEGER, \
            r REAL, \
            s TEXT, \
            b BLOB, \
            n INTEGER \
         )",
    )
    .await;
    exec_ok(
        conn,
        "INSERT INTO t (i, r, s, b, n) VALUES (42, 3.5, 'héllo', x'1234', NULL)",
    )
    .await;

    let (outcome, sink) = run(conn, "SELECT i, r, s, b, n FROM t", &ExecOptions::default()).await;
    assert_eq!(outcome.result_sets, 1);
    assert_eq!(outcome.total_rows, 1);
    assert!(!outcome.truncated);

    let sets = sink.sets();
    let set = &sets[0];
    let names: Vec<&str> = set.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["i", "r", "s", "b", "n"]);

    let row = &set.rows[0];
    assert_eq!(row[0], CellValue::I64(42), "INTEGER -> I64");
    assert_eq!(row[1], CellValue::F64(3.5), "REAL -> F64");
    assert_eq!(
        row[2],
        CellValue::String("héllo".to_string()),
        "TEXT -> String"
    );
    assert_eq!(row[3], CellValue::Bytes(vec![0x12, 0x34]), "BLOB -> Bytes");
    assert_eq!(row[4], CellValue::Null, "NULL -> Null");

    // Declared-type logical bucketing on the column metadata.
    assert_eq!(set.columns[0].logical, LogicalType::Integer);
    assert_eq!(set.columns[1].logical, LogicalType::Float);
    assert_eq!(set.columns[2].logical, LogicalType::Text);
    assert_eq!(set.columns[3].logical, LogicalType::Binary);
}

// ---------------------------------------------------------------------------
// 3. many rows + batching/order
// ---------------------------------------------------------------------------

#[tokio::test]
async fn many_rows_are_batched_and_ordered() {
    let mut fixture = start_sqlite().await;
    let conn = fixture.conn.as_mut();

    exec_ok(conn, "CREATE TABLE nums (id INTEGER PRIMARY KEY)").await;
    // A recursive CTE fills 0..1500 in one statement.
    exec_ok(
        conn,
        "WITH RECURSIVE n(id) AS ( \
            SELECT 0 UNION ALL SELECT id + 1 FROM n WHERE id < 1499 \
         ) \
         INSERT INTO nums (id) SELECT id FROM n",
    )
    .await;

    let opts = ExecOptions {
        max_rows: None,
        batch_size: 100,
    };
    let (outcome, sink) = run(conn, "SELECT id FROM nums ORDER BY id", &opts).await;

    assert_eq!(outcome.total_rows, 1500, "all rows delivered");
    assert_eq!(outcome.result_sets, 1);
    assert!(!outcome.truncated);

    let sets = sink.sets();
    let set = &sets[0];
    assert_eq!(set.rows.len(), 1500);
    assert!(
        set.batch_count >= 15,
        "expected >= 15 batches at batch_size=100, got {}",
        set.batch_count
    );
    assert_eq!(set.set_end_count, 1, "exactly one on_set_end for the set");

    for (expected, row) in set.rows.iter().enumerate() {
        assert_eq!(
            row[0],
            CellValue::I64(expected as i64),
            "row {expected} out of order"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. max_rows truncation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_rows_truncates_at_the_cap() {
    let mut fixture = start_sqlite().await;
    let conn = fixture.conn.as_mut();

    exec_ok(conn, "CREATE TABLE nums (id INTEGER PRIMARY KEY)").await;
    exec_ok(
        conn,
        "WITH RECURSIVE n(id) AS ( \
            SELECT 0 UNION ALL SELECT id + 1 FROM n WHERE id < 1499 \
         ) \
         INSERT INTO nums (id) SELECT id FROM n",
    )
    .await;

    let opts = ExecOptions {
        max_rows: Some(500),
        batch_size: 100,
    };
    let (outcome, sink) = run(conn, "SELECT id FROM nums ORDER BY id", &opts).await;

    assert!(outcome.truncated, "hitting max_rows must set truncated");
    assert_eq!(outcome.total_rows, 500, "exactly max_rows in the outcome");
    assert_eq!(sink.total_rows(), 500, "exactly 500 rows delivered");
}

// ---------------------------------------------------------------------------
// 5. cancellation (pre-cancelled token)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn precancelled_token_yields_cancelled() {
    let mut fixture = start_sqlite().await;
    let conn = fixture.conn.as_mut();
    exec_ok(conn, "CREATE TABLE t (id INTEGER)").await;
    exec_ok(conn, "INSERT INTO t (id) VALUES (1), (2), (3)").await;

    let cancel = CancelToken::new();
    cancel.cancel(); // already cancelled before execution

    let mut sink = CollectingSink::new();
    let err = conn
        .execute(
            "SELECT id FROM t",
            &ExecOptions::default(),
            &mut sink,
            &cancel,
        )
        .await
        .expect_err("a pre-cancelled execution returns Err");
    assert!(
        matches!(err, CoreError::Cancelled),
        "expected CoreError::Cancelled, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. DML reports affected count as a column-less result set
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dml_reports_rows_affected_as_columnless_set() {
    let mut fixture = start_sqlite().await;
    let conn = fixture.conn.as_mut();

    exec_ok(
        conn,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, done INTEGER)",
    )
    .await;
    exec_ok(conn, "INSERT INTO t (id, done) VALUES (1,0),(2,0),(3,0)").await;

    let (outcome, sink) = run(
        conn,
        "UPDATE t SET done = 1 WHERE id IN (1, 2)",
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.result_sets, 1);
    assert_eq!(outcome.total_rows, 0);
    let set = &sink.sets()[0];
    assert!(set.columns.is_empty(), "DML count set has no columns");
    assert!(set.rows.is_empty(), "DML count set has no row data");
    assert_eq!(set.affected, vec![Some(2)], "two rows updated");
}

// ---------------------------------------------------------------------------
// 7. introspection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn introspection_lists_databases_schemas_tables_columns() {
    let mut fixture = start_sqlite().await;
    let conn = fixture.conn.as_mut();

    exec_ok(
        conn,
        "CREATE TABLE customers ( \
            id INTEGER NOT NULL PRIMARY KEY, \
            name TEXT NOT NULL, \
            email TEXT, \
            balance NUMERIC \
         )",
    )
    .await;
    exec_ok(
        conn,
        "CREATE VIEW v_customers AS SELECT id, name FROM customers",
    )
    .await;

    // --- databases: `main` is always present ---
    let dbs = conn.list_databases().await.expect("list_databases");
    assert!(
        dbs.iter().any(|d| d.name == "main"),
        "main database expected, got {dbs:?}"
    );
    for d in &dbs {
        assert!(!d.is_system, "SQLite reports no system databases");
        assert_eq!(d.state_desc, "ONLINE");
    }

    // --- schemas: empty (no schema level) ---
    let schemas = conn.list_schemas("main").await.expect("list_schemas");
    assert!(schemas.is_empty(), "SQLite has no schema level");

    // --- tables + views ---
    let tables = conn.list_tables("main", "").await.expect("list_tables");
    let table = tables
        .iter()
        .find(|t| t.name == "customers")
        .expect("customers table present");
    assert_eq!(table.kind, TableKind::Table);
    let view = tables
        .iter()
        .find(|t| t.name == "v_customers")
        .expect("v_customers view present");
    assert_eq!(view.kind, TableKind::View);
    assert!(
        !tables.iter().any(|t| t.name.starts_with("sqlite_")),
        "internal sqlite_* objects must be excluded"
    );

    // --- columns ---
    let cols = conn
        .list_columns("main", "", "customers")
        .await
        .expect("list_columns");
    let by_name = |n: &str| {
        cols.iter()
            .find(|c| c.name == n)
            .unwrap_or_else(|| panic!("column {n} missing; got {cols:?}"))
    };

    let id = by_name("id");
    assert_eq!(id.data_type, "INTEGER");
    assert!(!id.nullable, "id is NOT NULL");
    assert!(id.is_primary_key, "id is the primary key");

    let name = by_name("name");
    assert_eq!(name.data_type, "TEXT");
    assert!(!name.nullable, "name is NOT NULL");
    assert!(!name.is_primary_key);

    let email = by_name("email");
    assert!(email.nullable, "email is NULL-able");
    assert!(!email.is_primary_key);

    let balance = by_name("balance");
    assert_eq!(balance.data_type, "NUMERIC");

    assert_eq!(
        cols.iter().filter(|c| c.is_primary_key).count(),
        1,
        "exactly one primary-key column"
    );
}

// ---------------------------------------------------------------------------
// 8. CSV-style import via create_table + import_rows (happy path)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_creates_table_and_inserts_typed_rows() {
    let mut fixture = start_sqlite().await;
    let conn = fixture.conn.as_mut();

    let columns = vec![
        selene_core::NewColumn {
            name: "id".into(),
            sql_type: "INTEGER".into(),
            nullable: true,
        },
        selene_core::NewColumn {
            name: "name".into(),
            sql_type: "TEXT".into(),
            nullable: true,
        },
        selene_core::NewColumn {
            // TEXT affinity preserves the exact decimal string. (A NUMERIC
            // column would apply SQLite's numeric affinity and coerce "10.50"
            // to the REAL 10.5 — correct SQLite behaviour, but it would not
            // demonstrate the lossless decimal-as-text binding under test.)
            name: "amount".into(),
            sql_type: "TEXT".into(),
            nullable: true,
        },
    ];
    conn.create_table(None, "", "imported", &columns, &CancelToken::new())
        .await
        .expect("create_table");

    // Decimal binds as TEXT and round-trips exactly.
    let mut source = VecRowSource::new(vec![
        vec![
            CellValue::I64(1),
            CellValue::String("alpha".into()),
            CellValue::Decimal("10.50".into()),
        ],
        vec![
            CellValue::I64(2),
            CellValue::String("beta".into()),
            CellValue::Decimal("20.25".into()),
        ],
    ]);
    let target = ImportTarget {
        database: None,
        schema: String::new(),
        table: "imported".into(),
        columns: vec!["id".into(), "name".into(), "amount".into()],
    };
    let inserted = conn
        .import_rows(&target, &mut source, true, 500, &CancelToken::new())
        .await
        .expect("import_rows");
    assert_eq!(inserted, 2);

    let (_outcome, sink) = run(
        conn,
        "SELECT id, name, amount FROM imported ORDER BY id",
        &ExecOptions::default(),
    )
    .await;
    let rows = sink.sets()[0].rows.clone();
    assert_eq!(
        rows,
        vec![
            vec![
                CellValue::I64(1),
                CellValue::String("alpha".into()),
                // Bound as TEXT into a TEXT column: the exact decimal string is
                // preserved (no precision loss).
                CellValue::String("10.50".into()),
            ],
            vec![
                CellValue::I64(2),
                CellValue::String("beta".into()),
                CellValue::String("20.25".into()),
            ],
        ],
    );
}

// ---------------------------------------------------------------------------
// 9. atomic import rolls back on a bad row
// ---------------------------------------------------------------------------

/// A source whose second batch errors, to exercise the atomic rollback path.
struct FailingRowSource {
    first: Option<Vec<Vec<CellValue>>>,
}

#[async_trait]
impl RowSource for FailingRowSource {
    async fn next_batch(&mut self) -> Result<Vec<Vec<CellValue>>, CoreError> {
        match self.first.take() {
            Some(rows) => Ok(rows),
            None => Err(CoreError::Import("simulated bad row".into())),
        }
    }
}

#[tokio::test]
async fn atomic_import_rolls_back_on_bad_row() {
    let mut fixture = start_sqlite().await;
    let conn = fixture.conn.as_mut();
    exec_ok(conn, "CREATE TABLE nums (n INTEGER)").await;

    // First batch inserts a row; the next pull errors, so the whole atomic
    // import must roll back, leaving the table empty.
    let mut source = FailingRowSource {
        first: Some(vec![vec![CellValue::I64(1)]]),
    };
    let target = ImportTarget {
        database: None,
        schema: String::new(),
        table: "nums".into(),
        columns: vec!["n".into()],
    };
    let err = conn
        .import_rows(&target, &mut source, true, 500, &CancelToken::new())
        .await
        .expect_err("a failing source aborts an atomic import");
    assert!(matches!(err, CoreError::Import(_)), "got {err:?}");

    let (_o, sink) = run(
        conn,
        "SELECT COUNT(*) AS n FROM nums",
        &ExecOptions::default(),
    )
    .await;
    assert_eq!(
        sink.sets()[0].rows[0][0],
        CellValue::I64(0),
        "the transaction must have rolled back, leaving zero rows"
    );
}

// ---------------------------------------------------------------------------
// 10. drop_table clears a conflict so a re-create can recreate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drop_table_clears_a_conflict_for_recreate() {
    let mut fixture = start_sqlite().await;
    let conn = fixture.conn.as_mut();

    let columns = vec![selene_core::NewColumn {
        name: "id".into(),
        sql_type: "INTEGER".into(),
        nullable: true,
    }];

    conn.create_table(None, "", "imported", &columns, &CancelToken::new())
        .await
        .expect("first create_table");

    // Re-creating the same table fails ("table imported already exists").
    let err = conn
        .create_table(None, "", "imported", &columns, &CancelToken::new())
        .await
        .expect_err("recreating an existing table must fail");
    assert!(matches!(err, CoreError::Query(_)), "got {err:?}");

    // Dropping clears the conflict so the retry can recreate it.
    conn.drop_table(None, "", "imported", &CancelToken::new())
        .await
        .expect("drop_table");
    conn.create_table(None, "", "imported", &columns, &CancelToken::new())
        .await
        .expect("create_table after drop");
}

// ---------------------------------------------------------------------------
// 11. multiple statements: a SELECT followed by a DML in one batch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn select_then_dml_in_one_batch() {
    let mut fixture = start_sqlite().await;
    let conn = fixture.conn.as_mut();
    exec_ok(conn, "CREATE TABLE t (id INTEGER)").await;
    exec_ok(conn, "INSERT INTO t (id) VALUES (1), (2)").await;

    // A SELECT (one set) followed by an UPDATE (a column-less count set).
    let (outcome, sink) = run(
        conn,
        "SELECT id FROM t ORDER BY id; UPDATE t SET id = id + 10;",
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.result_sets, 2, "a read set + a DML count set");
    let sets = sink.sets();
    assert_eq!(
        sets[0]
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["id"]
    );
    assert_eq!(
        sets[0].rows,
        vec![vec![CellValue::I64(1)], vec![CellValue::I64(2)]]
    );
    assert!(sets[1].columns.is_empty(), "the DML set is column-less");
    assert_eq!(sets[1].affected, vec![Some(2)], "two rows updated");
}
