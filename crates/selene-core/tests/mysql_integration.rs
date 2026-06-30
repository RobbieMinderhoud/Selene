//! Integration tests for the MySQL driver against a **real** MySQL server.
//!
//! These spin up the official `mysql` image in Docker via [`testcontainers`]
//! (the `mysql` module of `testcontainers-modules`) and exercise `selene-core`'s
//! public driver API end-to-end: connect, typed scalar conversion (including the
//! `BIGINT UNSIGNED` → lossless-decimal fallback), batched streaming, the
//! `max_rows` cap, cooperative cancellation, schema introspection (no schema
//! level), and a CSV-style import round-trip.
//!
//! ## Why every test is `#[ignore]`-d
//! A plain `cargo test` must stay hermetic and fast, so these are gated behind
//! `--ignored`:
//!
//! ```text
//! cargo test -p selene-core --features mysql --test mysql_integration -- --ignored
//! ```
//!
//! testcontainers maps the container's internal `3306` to a **random** host port
//! (`get_host_port_ipv4(3306)`), so there is no conflict with any local MySQL
//! instance — the port is never hardcoded. The module starts MySQL with the
//! `root` user, an **empty** password (`MYSQL_ALLOW_EMPTY_PASSWORD=yes`), and a
//! default database named `test`.
//!
//! MySQL containers take noticeably longer to become ready than Postgres, and the
//! server briefly accepts then drops early connections during its init phase, so
//! [`connect_with_retry`] retries the first connect with a short backoff.

#![cfg(feature = "mysql")]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::mysql::Mysql;

use selene_core::driver::{driver_for, DatabaseDriver};
use selene_core::ConnectionSpec;
use selene_core::{
    AuthMethod, CancelToken, CellValue, Column, Connection, CoreError, DriverId, ExecOptions, Flow,
    ImportTarget, LogicalType, NewColumn, RowSink, RowSource, Secret, TableKind, TemporalKind,
    TlsConfig,
};

/// The default credentials/database the `mysql` testcontainers module sets:
/// the `root` user with an empty password and a `test` database.
const MY_USER: &str = "root";
const MY_PASSWORD: &str = "";
const MY_DATABASE: &str = "test";

/// A live test fixture: the connected `Connection` plus the running container.
///
/// The `ContainerAsync` guard MUST be held for the lifetime of the test —
/// dropping it stops and removes the container (and so kills the connection).
struct Fixture {
    conn: Box<dyn Connection>,
    // Kept alive to keep the container running; never read directly.
    _container: ContainerAsync<Mysql>,
}

/// Build a spec pointing at the container on `port`.
fn spec_for(port: u16) -> ConnectionSpec {
    ConnectionSpec {
        id: "it-mysql".to_string(),
        name: "integration".to_string(),
        driver: DriverId::Mysql,
        host: "127.0.0.1".to_string(),
        port: Some(port),
        instance: None,
        database: Some(MY_DATABASE.to_string()),
        auth: AuthMethod::SqlLogin {
            username: MY_USER.to_string(),
        },
        // The default image speaks plaintext; `Preferred` (encrypt=false) lets the
        // handshake fall back cleanly rather than requiring TLS the server is not
        // configured for.
        tls: TlsConfig {
            encrypt: false,
            trust_server_certificate: false,
        },
        read_only: false,
    }
}

/// Open a connection, retrying a few times with a short backoff.
///
/// Even after testcontainers reports the readiness log lines, a freshly-started
/// MySQL can refuse or immediately drop the first connection or two while it
/// finishes initialising, so a bare single `connect` is flaky. Retrying for a few
/// seconds makes the suite reliable without papering over a genuine failure.
async fn connect_with_retry(
    driver: &dyn DatabaseDriver,
    spec: &ConnectionSpec,
    secret: &Secret,
) -> Box<dyn Connection> {
    let mut last_err: Option<CoreError> = None;
    // ~15s of attempts (30 × 500ms) covers a slow first-connect window.
    for _ in 0..30 {
        match driver.connect(spec, secret).await {
            Ok(conn) => return conn,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    panic!("could not connect to MySQL after retries: {last_err:?}");
}

/// Start a fresh MySQL container and open a `selene-core` connection to it.
async fn start_mysql() -> Fixture {
    let container = Mysql::default()
        .start()
        .await
        .expect("start MySQL container");

    let port = container
        .get_host_port_ipv4(3306)
        .await
        .expect("map container port 3306 to a host port");

    let driver = driver_for(DriverId::Mysql).expect("mysql driver compiled in");
    let conn =
        connect_with_retry(driver.as_ref(), &spec_for(port), &Secret::new(MY_PASSWORD)).await;

    Fixture {
        conn,
        _container: container,
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

/// A [`RowSink`] recording everything per `set_index` — mirrors the mssql/sqlite/
/// postgres test sinks so the assertions read the same way.
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
        let mut sets = self.sets.lock().unwrap();
        Self::ensure(&mut sets, set_index);
        let set = &mut sets[set_index];
        set.batch_count += 1;
        set.rows.extend(rows);
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

/// Create a `seq(id)` table filled with `0..count`.
///
/// MySQL has no `generate_series`, and a recursive CTE defaults to a 1000-row
/// recursion cap (`@@cte_max_recursion_depth`), so we raise the session limit
/// before running it. Used by the batching and truncation tests.
async fn fill_seq(conn: &mut dyn Connection, count: i64) {
    exec_ok(conn, "CREATE TABLE seq (id INT PRIMARY KEY)").await;
    exec_ok(conn, "SET SESSION cte_max_recursion_depth = 100000").await;
    exec_ok(
        conn,
        &format!(
            "INSERT INTO seq (id) \
             WITH RECURSIVE g(n) AS ( \
                SELECT 0 UNION ALL SELECT n + 1 FROM g WHERE n < {} \
             ) SELECT n FROM g",
            count - 1
        ),
    )
    .await;
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
#[ignore = "requires Docker; run with --ignored"]
async fn connect_and_test_connection_reports_version() {
    let container = Mysql::default()
        .start()
        .await
        .expect("start MySQL container");
    let port = container.get_host_port_ipv4(3306).await.expect("map port");

    let driver = driver_for(DriverId::Mysql).unwrap();
    let spec = spec_for(port);
    let secret = Secret::new(MY_PASSWORD);

    // Reuse the retry policy: test_connection can hit the same early-drop window.
    let mut report = None;
    for _ in 0..30 {
        match driver.test_connection(&spec, &secret).await {
            Ok(r) => {
                report = Some(r);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
    let report = report.expect("test_connection succeeds within the retry window");

    let version = report
        .server_version
        .expect("server_version should be Some");
    // MySQL reports e.g. "8.1.0"; just assert it is non-empty and digit-led.
    assert!(
        version.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "unexpected version banner: {version}"
    );
}

// ---------------------------------------------------------------------------
// 2. typed scalar SELECT (static type mapping) across int widths + families
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn typed_scalar_select_maps_every_type() {
    let mut fixture = start_mysql().await;
    let conn = fixture.conn.as_mut();

    // Build a table covering every supported family + the BIGINT UNSIGNED tail.
    exec_ok(
        conn,
        "CREATE TABLE scalars ( \
            c_ti   TINYINT, \
            c_si   SMALLINT, \
            c_mi   MEDIUMINT, \
            c_i    INT, \
            c_bi   BIGINT, \
            c_ubig BIGINT UNSIGNED, \
            c_bool TINYINT(1), \
            c_f    FLOAT, \
            c_d    DOUBLE, \
            c_dec  DECIMAL(10,4), \
            c_dt   DATETIME, \
            c_ts   TIMESTAMP NULL, \
            c_date DATE, \
            c_time TIME, \
            c_json JSON, \
            c_blob BLOB, \
            c_vc   VARCHAR(32), \
            c_null INT \
         )",
    )
    .await;

    // 18446744073709551615 = u64::MAX, above i64::MAX → must use the Decimal path.
    exec_ok(
        conn,
        "INSERT INTO scalars VALUES ( \
            1, 2, 3, 4, 5, 18446744073709551615, \
            1, 1.5, 2.5, 123.4500, \
            '2026-06-30 12:00:00', '2026-06-30 12:00:00', '2026-06-30', '08:09:10', \
            '{\"a\": 1}', 0xDEADBEEF, 'héllo', NULL \
         )",
    )
    .await;

    let (outcome, sink) = run(
        conn,
        "SELECT c_ti, c_si, c_mi, c_i, c_bi, c_ubig, c_bool, c_f, c_d, c_dec, \
                c_dt, c_ts, c_date, c_time, c_json, c_blob, c_vc, c_null \
         FROM scalars",
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.result_sets, 1);
    assert_eq!(outcome.total_rows, 1);
    assert!(!outcome.truncated);

    let sets = sink.sets();
    let set = &sets[0];
    let row = &set.rows[0];

    assert_eq!(row[0], CellValue::I64(1), "tinyint -> I64");
    assert_eq!(row[1], CellValue::I64(2), "smallint -> I64");
    assert_eq!(row[2], CellValue::I64(3), "mediumint -> I64");
    assert_eq!(row[3], CellValue::I64(4), "int -> I64");
    assert_eq!(row[4], CellValue::I64(5), "bigint -> I64");
    assert_eq!(
        row[5],
        CellValue::Decimal("18446744073709551615".to_string()),
        "bigint unsigned above i64::MAX -> lossless Decimal"
    );
    assert_eq!(row[6], CellValue::Bool(true), "tinyint(1) -> Bool");
    assert_eq!(row[7], CellValue::F64(1.5), "float -> F64");
    assert_eq!(row[8], CellValue::F64(2.5), "double -> F64");
    assert_eq!(
        row[9],
        CellValue::Decimal("123.4500".to_string()),
        "decimal -> lossless decimal string"
    );
    assert_eq!(
        row[10],
        CellValue::DateTime {
            iso: "2026-06-30T12:00:00".to_string(),
            kind: TemporalKind::DateTime,
        },
        "datetime -> naive datetime"
    );
    assert_eq!(
        row[11],
        CellValue::DateTime {
            iso: "2026-06-30T12:00:00".to_string(),
            kind: TemporalKind::DateTime,
        },
        "timestamp -> naive datetime (no offset)"
    );
    assert_eq!(
        row[12],
        CellValue::DateTime {
            iso: "2026-06-30".to_string(),
            kind: TemporalKind::Date,
        },
        "date -> Date"
    );
    assert_eq!(
        row[13],
        CellValue::DateTime {
            iso: "08:09:10".to_string(),
            kind: TemporalKind::Time,
        },
        "time -> Time"
    );
    // MySQL stores JSON normalised; serde_json::Value::to_string is compact.
    assert_eq!(
        row[14],
        CellValue::String("{\"a\":1}".to_string()),
        "json -> compact String document"
    );
    assert_eq!(
        row[15],
        CellValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        "blob -> Bytes"
    );
    assert_eq!(
        row[16],
        CellValue::String("héllo".to_string()),
        "varchar -> String"
    );
    assert_eq!(row[17], CellValue::Null, "NULL -> Null");

    // Column logical bucketing.
    assert_eq!(set.columns[0].logical, LogicalType::Integer, "tinyint");
    assert_eq!(set.columns[5].logical, LogicalType::Integer, "bigint uns");
    assert_eq!(set.columns[6].logical, LogicalType::Boolean, "tinyint(1)");
    assert_eq!(set.columns[7].logical, LogicalType::Float, "float");
    assert_eq!(set.columns[9].logical, LogicalType::Decimal, "decimal");
    assert_eq!(set.columns[10].logical, LogicalType::DateTime, "datetime");
    assert_eq!(set.columns[12].logical, LogicalType::Date, "date");
    assert_eq!(set.columns[13].logical, LogicalType::Time, "time");
    assert_eq!(set.columns[14].logical, LogicalType::Json, "json");
    assert_eq!(set.columns[15].logical, LogicalType::Binary, "blob");
    assert_eq!(set.columns[16].logical, LogicalType::Text, "varchar");
}

// ---------------------------------------------------------------------------
// 3. many rows + batching/order
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn many_rows_are_batched_and_ordered() {
    let mut fixture = start_mysql().await;
    let conn = fixture.conn.as_mut();

    fill_seq(conn, 1500).await;

    let opts = ExecOptions {
        max_rows: None,
        batch_size: 100,
    };
    let (outcome, sink) = run(conn, "SELECT id FROM seq ORDER BY id", &opts).await;

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
#[ignore = "requires Docker; run with --ignored"]
async fn max_rows_truncates_at_the_cap() {
    let mut fixture = start_mysql().await;
    let conn = fixture.conn.as_mut();

    fill_seq(conn, 1500).await;

    let opts = ExecOptions {
        max_rows: Some(500),
        batch_size: 100,
    };
    let (outcome, sink) = run(conn, "SELECT id FROM seq ORDER BY id", &opts).await;

    assert!(outcome.truncated, "hitting max_rows must set truncated");
    assert_eq!(outcome.total_rows, 500, "exactly max_rows in the outcome");
    assert_eq!(sink.total_rows(), 500, "exactly 500 rows delivered");
}

// ---------------------------------------------------------------------------
// 5. cancellation (pre-cancelled token)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn precancelled_token_yields_cancelled() {
    let mut fixture = start_mysql().await;
    let conn = fixture.conn.as_mut();

    let cancel = CancelToken::new();
    cancel.cancel(); // already cancelled before execution

    let mut sink = CollectingSink::new();
    let err = conn
        .execute("SELECT 1", &ExecOptions::default(), &mut sink, &cancel)
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
#[ignore = "requires Docker; run with --ignored"]
async fn dml_reports_rows_affected_as_columnless_set() {
    let mut fixture = start_mysql().await;
    let conn = fixture.conn.as_mut();

    exec_ok(conn, "CREATE TABLE t (id INT PRIMARY KEY, done INT)").await;
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
// 7. introspection — NO schema level; a table + a view; columns with PK
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn introspection_lists_databases_tables_columns_no_schema_level() {
    let mut fixture = start_mysql().await;
    let conn = fixture.conn.as_mut();

    exec_ok(
        conn,
        "CREATE TABLE customers ( \
            id INT NOT NULL PRIMARY KEY, \
            name VARCHAR(255) NOT NULL, \
            email TEXT, \
            balance DECIMAL(10,2) \
         )",
    )
    .await;
    exec_ok(
        conn,
        "CREATE VIEW v_customers AS SELECT id, name FROM customers",
    )
    .await;

    // --- databases: the `test` database is listed; system DBs are excluded ---
    let dbs = conn.list_databases().await.expect("list_databases");
    assert!(
        dbs.iter().any(|d| d.name == MY_DATABASE),
        "the connected database should be listed, got {dbs:?}"
    );
    assert!(
        !dbs.iter().any(|d| {
            matches!(
                d.name.as_str(),
                "mysql" | "information_schema" | "performance_schema" | "sys"
            )
        }),
        "system databases must be excluded, got {dbs:?}"
    );
    for d in &dbs {
        assert!(!d.is_system);
        assert_eq!(d.state_desc, "ONLINE");
    }

    // --- schemas: MySQL has no schema level, so this is always empty ---
    let schemas = conn.list_schemas(MY_DATABASE).await.expect("list_schemas");
    assert!(
        schemas.is_empty(),
        "MySQL has no schema level; expected an empty list, got {schemas:?}"
    );

    // --- tables + views (schema arg is a placeholder, ignored) ---
    let tables = conn
        .list_tables(MY_DATABASE, "")
        .await
        .expect("list_tables");
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

    // --- columns ---
    let cols = conn
        .list_columns(MY_DATABASE, "", "customers")
        .await
        .expect("list_columns");
    let by_name = |n: &str| {
        cols.iter()
            .find(|c| c.name == n)
            .unwrap_or_else(|| panic!("column {n} missing; got {cols:?}"))
    };

    let id = by_name("id");
    assert!(!id.nullable, "id is NOT NULL");
    assert!(id.is_primary_key, "id is the primary key");

    let name = by_name("name");
    assert!(!name.nullable, "name is NOT NULL");
    assert!(!name.is_primary_key);
    assert_eq!(name.max_length, Some(255), "varchar(255) length reported");

    let email = by_name("email");
    assert!(email.nullable, "email is NULL-able");
    assert!(!email.is_primary_key);

    assert_eq!(
        cols.iter().filter(|c| c.is_primary_key).count(),
        1,
        "exactly one primary-key column"
    );
}

// ---------------------------------------------------------------------------
// 8. use_database switches the active database
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn use_database_switches_current_database() {
    let mut fixture = start_mysql().await;
    let conn = fixture.conn.as_mut();

    // Connected to `test` initially.
    assert_eq!(
        conn.current_database().await.expect("current_database"),
        MY_DATABASE
    );

    // Create a second database and switch to it.
    exec_ok(conn, "CREATE DATABASE other_db").await;
    conn.use_database("other_db")
        .await
        .expect("use_database succeeds on MySQL");
    assert_eq!(
        conn.current_database().await.expect("current_database"),
        "other_db",
        "the active database should follow USE"
    );
}

// ---------------------------------------------------------------------------
// 9. CSV-style import via create_table + import_rows (happy path)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn import_creates_table_and_inserts_typed_rows() {
    let mut fixture = start_mysql().await;
    let conn = fixture.conn.as_mut();

    let columns = vec![
        NewColumn {
            name: "id".into(),
            sql_type: "INT".into(),
            nullable: true,
        },
        NewColumn {
            name: "name".into(),
            sql_type: "VARCHAR(64)".into(),
            nullable: true,
        },
        NewColumn {
            name: "amount".into(),
            sql_type: "DECIMAL(10,2)".into(),
            nullable: true,
        },
    ];
    conn.create_table(None, "", "imported", &columns, &CancelToken::new())
        .await
        .expect("create_table");

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
                // decimal column: the exact decimal string is preserved.
                CellValue::Decimal("10.50".into()),
            ],
            vec![
                CellValue::I64(2),
                CellValue::String("beta".into()),
                CellValue::Decimal("20.25".into()),
            ],
        ],
    );
}

// ---------------------------------------------------------------------------
// 10. atomic import rolls back on a bad row
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
#[ignore = "requires Docker; run with --ignored"]
async fn atomic_import_rolls_back_on_bad_row() {
    let mut fixture = start_mysql().await;
    let conn = fixture.conn.as_mut();
    // InnoDB (the default engine) is required for the rollback to take effect.
    exec_ok(conn, "CREATE TABLE nums (n INT) ENGINE=InnoDB").await;

    // First batch inserts a row; the next pull errors, so the whole atomic import
    // must roll back, leaving the table empty.
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
// 11. drop_table clears a conflict so a re-create can recreate
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn drop_table_clears_a_conflict_for_recreate() {
    let mut fixture = start_mysql().await;
    let conn = fixture.conn.as_mut();

    let columns = vec![NewColumn {
        name: "id".into(),
        sql_type: "INT".into(),
        nullable: true,
    }];

    conn.create_table(None, "", "imported", &columns, &CancelToken::new())
        .await
        .expect("first create_table");

    // Re-creating the same table fails (table already exists).
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
