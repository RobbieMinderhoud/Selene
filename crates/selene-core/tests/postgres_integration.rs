//! Integration tests for the PostgreSQL driver against a **real** Postgres server.
//!
//! These spin up the official `postgres` image in Docker via [`testcontainers`]
//! (the `postgres` module of `testcontainers-modules`) and exercise
//! `selene-core`'s public driver API end-to-end: connect, typed scalar
//! conversion, batched streaming, the `max_rows` cap, multiple statements,
//! cooperative cancellation, schema introspection, and a CSV-style import
//! round-trip.
//!
//! ## Why every test is `#[ignore]`-d
//! A plain `cargo test` must stay hermetic and fast, so these are gated behind
//! `--ignored`:
//!
//! ```text
//! cargo test -p selene-core --features postgres -- --ignored
//! ```
//!
//! testcontainers maps the container's internal `5432` to a **random** host port
//! (`get_host_port_ipv4(5432)`), so there is no conflict with any local Postgres
//! instance — the port is never hardcoded. The module's default database, user,
//! and password are all `postgres`.

#![cfg(feature = "postgres")]

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;

use selene_core::driver::driver_for;
use selene_core::ConnectionSpec;
use selene_core::{
    AuthMethod, CancelToken, CellValue, Column, Connection, CoreError, DriverId, ExecOptions, Flow,
    ImportTarget, LogicalType, NewColumn, RowSink, RowSource, Secret, TableKind, TemporalKind,
    TlsConfig,
};

/// The default credentials/database the `postgres` testcontainers module sets.
const PG_USER: &str = "postgres";
const PG_PASSWORD: &str = "postgres";
const PG_DATABASE: &str = "postgres";

/// A live test fixture: the connected `Connection` plus the running container.
///
/// The `ContainerAsync` guard MUST be held for the lifetime of the test —
/// dropping it stops and removes the container (and so kills the connection).
struct Fixture {
    conn: Box<dyn Connection>,
    // Kept alive to keep the container running; never read directly.
    _container: ContainerAsync<Postgres>,
}

/// Build a spec pointing at the container on `port`.
fn spec_for(port: u16) -> ConnectionSpec {
    ConnectionSpec {
        id: "it-postgres".to_string(),
        name: "integration".to_string(),
        driver: DriverId::Postgres,
        host: "127.0.0.1".to_string(),
        port: Some(port),
        instance: None,
        uri: None,
        database: Some(PG_DATABASE.to_string()),
        auth: AuthMethod::SqlLogin {
            username: PG_USER.to_string(),
        },
        // The default image speaks plaintext; `Prefer` (encrypt=false) lets the
        // handshake fall back cleanly rather than requiring TLS the server is not
        // configured for.
        tls: TlsConfig {
            encrypt: false,
            trust_server_certificate: false,
        },
        read_only: false,
    }
}

/// Start a fresh Postgres container and open a `selene-core` connection to it.
async fn start_postgres() -> Fixture {
    let container = Postgres::default()
        .start()
        .await
        .expect("start Postgres container");

    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("map container port 5432 to a host port");

    let driver = driver_for(DriverId::Postgres).expect("postgres driver compiled in");
    let conn = driver
        .connect(&spec_for(port), &Secret::new(PG_PASSWORD))
        .await
        .expect("connect to Postgres");

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

/// A [`RowSink`] recording everything per `set_index` — mirrors the mssql/sqlite
/// test sinks so the assertions read the same way.
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
    let container = Postgres::default()
        .start()
        .await
        .expect("start Postgres container");
    let port = container.get_host_port_ipv4(5432).await.expect("map port");

    let driver = driver_for(DriverId::Postgres).unwrap();
    let report = driver
        .test_connection(&spec_for(port), &Secret::new(PG_PASSWORD))
        .await
        .expect("test_connection succeeds");

    let version = report
        .server_version
        .expect("server_version should be Some");
    assert!(
        version.contains("PostgreSQL"),
        "unexpected version banner: {version}"
    );
}

// ---------------------------------------------------------------------------
// 2. typed scalar SELECT (static type mapping)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn typed_scalar_select_maps_every_type() {
    let mut fixture = start_postgres().await;
    let conn = fixture.conn.as_mut();

    // One column per supported type family, plus a NULL.
    let (outcome, sink) = run(
        conn,
        "SELECT \
            (1::int2)              AS c_i2, \
            (2::int4)              AS c_i4, \
            (3::int8)              AS c_i8, \
            (1.5::float4)          AS c_f4, \
            (2.5::float8)          AS c_f8, \
            (123.4500::numeric)    AS c_num, \
            (true)                 AS c_bool, \
            ('héllo'::text)        AS c_text, \
            ('vc'::varchar)        AS c_vc, \
            ('11111111-2222-3333-4444-555555555555'::uuid) AS c_uuid, \
            (TIMESTAMPTZ '2026-06-30 12:00:00+00') AS c_tstz, \
            (TIMESTAMP   '2026-06-30 12:00:00')    AS c_ts, \
            (DATE        '2026-06-30')             AS c_date, \
            (TIME        '08:09:10')               AS c_time, \
            ('{\"a\":1}'::json)    AS c_json, \
            ('{\"b\":2}'::jsonb)   AS c_jsonb, \
            ('\\xdeadbeef'::bytea) AS c_bytea, \
            (NULL::int4)           AS c_null",
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.result_sets, 1);
    assert_eq!(outcome.total_rows, 1);
    assert!(!outcome.truncated);

    let sets = sink.sets();
    let set = &sets[0];
    let row = &set.rows[0];

    assert_eq!(row[0], CellValue::I64(1), "int2 -> I64");
    assert_eq!(row[1], CellValue::I64(2), "int4 -> I64");
    assert_eq!(row[2], CellValue::I64(3), "int8 -> I64");
    assert_eq!(row[3], CellValue::F64(1.5), "float4 -> F64");
    assert_eq!(row[4], CellValue::F64(2.5), "float8 -> F64");
    assert_eq!(
        row[5],
        CellValue::Decimal("123.4500".to_string()),
        "numeric -> lossless decimal string"
    );
    assert_eq!(row[6], CellValue::Bool(true), "bool -> Bool");
    assert_eq!(
        row[7],
        CellValue::String("héllo".to_string()),
        "text -> String"
    );
    assert_eq!(
        row[8],
        CellValue::String("vc".to_string()),
        "varchar -> String"
    );
    assert_eq!(
        row[9],
        CellValue::Uuid("11111111-2222-3333-4444-555555555555".to_string()),
        "uuid -> Uuid"
    );
    assert_eq!(
        row[10],
        CellValue::DateTime {
            iso: "2026-06-30T12:00:00+00:00".to_string(),
            kind: TemporalKind::DateTimeOffset,
        },
        "timestamptz -> RFC3339 offset datetime"
    );
    assert_eq!(
        row[11],
        CellValue::DateTime {
            iso: "2026-06-30T12:00:00".to_string(),
            kind: TemporalKind::DateTime,
        },
        "timestamp -> naive datetime"
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
    assert_eq!(
        row[14],
        CellValue::String("{\"a\":1}".to_string()),
        "json -> String document"
    );
    assert_eq!(
        row[15],
        CellValue::String("{\"b\":2}".to_string()),
        "jsonb -> compact String document (serde_json::Value::to_string is compact)"
    );
    assert_eq!(
        row[16],
        CellValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        "bytea -> Bytes"
    );
    assert_eq!(row[17], CellValue::Null, "NULL -> Null");

    // Column logical bucketing.
    assert_eq!(set.columns[0].logical, LogicalType::Integer);
    assert_eq!(set.columns[3].logical, LogicalType::Float);
    assert_eq!(set.columns[5].logical, LogicalType::Decimal);
    assert_eq!(set.columns[6].logical, LogicalType::Boolean);
    assert_eq!(set.columns[7].logical, LogicalType::Text);
    assert_eq!(set.columns[9].logical, LogicalType::Uuid);
    assert_eq!(set.columns[10].logical, LogicalType::DateTime);
    assert_eq!(set.columns[12].logical, LogicalType::Date);
    assert_eq!(set.columns[13].logical, LogicalType::Time);
    assert_eq!(set.columns[14].logical, LogicalType::Json);
    assert_eq!(set.columns[15].logical, LogicalType::Json);
    assert_eq!(set.columns[16].logical, LogicalType::Binary);
}

// ---------------------------------------------------------------------------
// 3. many rows + batching/order
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn many_rows_are_batched_and_ordered() {
    let mut fixture = start_postgres().await;
    let conn = fixture.conn.as_mut();

    let opts = ExecOptions {
        max_rows: None,
        batch_size: 100,
    };
    // generate_series fills 0..1500 in one statement.
    let (outcome, sink) = run(
        conn,
        "SELECT g AS id FROM generate_series(0, 1499) AS g ORDER BY id",
        &opts,
    )
    .await;

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
    let mut fixture = start_postgres().await;
    let conn = fixture.conn.as_mut();

    let opts = ExecOptions {
        max_rows: Some(500),
        batch_size: 100,
    };
    let (outcome, sink) = run(
        conn,
        "SELECT g AS id FROM generate_series(0, 1499) AS g ORDER BY id",
        &opts,
    )
    .await;

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
    let mut fixture = start_postgres().await;
    let conn = fixture.conn.as_mut();

    let cancel = CancelToken::new();
    cancel.cancel(); // already cancelled before execution

    let mut sink = CollectingSink::new();
    let err = conn
        .execute(
            "SELECT g FROM generate_series(1, 3) AS g",
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
// 6. multiple statements: a SELECT followed by a DML in one batch
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn select_then_dml_in_one_batch() {
    let mut fixture = start_postgres().await;
    let conn = fixture.conn.as_mut();
    exec_ok(conn, "CREATE TABLE t (id int4)").await;
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

// ---------------------------------------------------------------------------
// 7. DML reports affected count as a column-less result set
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn dml_reports_rows_affected_as_columnless_set() {
    let mut fixture = start_postgres().await;
    let conn = fixture.conn.as_mut();

    exec_ok(conn, "CREATE TABLE t (id int4 PRIMARY KEY, done int4)").await;
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
// 8. introspection
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn introspection_lists_database_schemas_tables_columns() {
    let mut fixture = start_postgres().await;
    let conn = fixture.conn.as_mut();

    exec_ok(
        conn,
        "CREATE TABLE customers ( \
            id int4 NOT NULL PRIMARY KEY, \
            name varchar(255) NOT NULL, \
            email text, \
            balance numeric \
         )",
    )
    .await;
    exec_ok(
        conn,
        "CREATE VIEW v_customers AS SELECT id, name FROM customers",
    )
    .await;

    // --- databases: exactly the connected database ---
    let dbs = conn.list_databases().await.expect("list_databases");
    assert_eq!(dbs.len(), 1, "only the connected database is listed");
    assert_eq!(dbs[0].name, PG_DATABASE);
    assert!(!dbs[0].is_system);
    assert_eq!(dbs[0].state_desc, "ONLINE");

    // --- schemas: includes `public`, excludes pg_* / information_schema ---
    let schemas = conn.list_schemas(PG_DATABASE).await.expect("list_schemas");
    assert!(
        schemas.iter().any(|s| s.name == "public"),
        "public schema expected, got {schemas:?}"
    );
    assert!(
        !schemas
            .iter()
            .any(|s| s.name.starts_with("pg_") || s.name == "information_schema"),
        "system schemas must be excluded, got {schemas:?}"
    );

    // --- tables + views ---
    let tables = conn
        .list_tables(PG_DATABASE, "public")
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
        .list_columns(PG_DATABASE, "public", "customers")
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
// 9. CSV-style import via create_table + import_rows (happy path)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn import_creates_table_and_inserts_typed_rows() {
    let mut fixture = start_postgres().await;
    let conn = fixture.conn.as_mut();

    let columns = vec![
        NewColumn {
            name: "id".into(),
            sql_type: "int4".into(),
            nullable: true,
        },
        NewColumn {
            name: "name".into(),
            sql_type: "text".into(),
            nullable: true,
        },
        NewColumn {
            // numeric(10,2) preserves the exact decimal; the value binds as TEXT
            // and Postgres coerces it to numeric on insert.
            name: "amount".into(),
            sql_type: "numeric".into(),
            nullable: true,
        },
    ];
    conn.create_table(None, "public", "imported", &columns, &CancelToken::new())
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
        schema: "public".into(),
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
                // numeric column: the exact decimal string is preserved.
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
    let mut fixture = start_postgres().await;
    let conn = fixture.conn.as_mut();
    exec_ok(conn, "CREATE TABLE nums (n int4)").await;

    // First batch inserts a row; the next pull errors, so the whole atomic
    // import must roll back, leaving the table empty.
    let mut source = FailingRowSource {
        first: Some(vec![vec![CellValue::I64(1)]]),
    };
    let target = ImportTarget {
        database: None,
        schema: "public".into(),
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
        "SELECT COUNT(*)::int4 AS n FROM nums",
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
    let mut fixture = start_postgres().await;
    let conn = fixture.conn.as_mut();

    let columns = vec![NewColumn {
        name: "id".into(),
        sql_type: "int4".into(),
        nullable: true,
    }];

    conn.create_table(None, "public", "imported", &columns, &CancelToken::new())
        .await
        .expect("first create_table");

    // Re-creating the same table fails (relation already exists).
    let err = conn
        .create_table(None, "public", "imported", &columns, &CancelToken::new())
        .await
        .expect_err("recreating an existing table must fail");
    assert!(matches!(err, CoreError::Query(_)), "got {err:?}");

    // Dropping clears the conflict so the retry can recreate it.
    conn.drop_table(None, "public", "imported", &CancelToken::new())
        .await
        .expect("drop_table");
    conn.create_table(None, "public", "imported", &columns, &CancelToken::new())
        .await
        .expect("create_table after drop");
}
