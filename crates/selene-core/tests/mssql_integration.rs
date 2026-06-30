//! Integration tests for the MSSQL driver against a **real** SQL Server.
//!
//! These spin up `mcr.microsoft.com/mssql/server` in Docker via
//! [`testcontainers`] (the `mssql_server` module of `testcontainers-modules`)
//! and exercise `selene-core`'s public driver API end-to-end: connect, typed
//! scalar conversion, batched streaming, the `max_rows` cap, multiple result
//! sets, cooperative cancellation, schema introspection, and a CSV export
//! round-trip.
//!
//! ## Why every test is `#[ignore]`-d
//! A plain `cargo test` must stay hermetic and fast (the 71 unit tests need no
//! Docker), so these are gated behind `--ignored`:
//!
//! ```text
//! cargo test -p selene-core -- --ignored
//! ```
//!
//! testcontainers maps the container's internal `1433` to a **random** host
//! port (`get_host_port_ipv4(1433)`), so there is no conflict with any local
//! SQL Server instance — the port is never hardcoded.
//!
//! ## Architecture note
//! SQL Server publishes amd64 images only; on Apple Silicon Docker Desktop runs
//! them under emulation. The first container start therefore takes ~20-40s
//! while the server recovers its system databases; the module's
//! `ready_conditions` wait for the "ready for client connections" and "Recovery
//! is complete" log lines before `start().await` returns.

#![cfg(feature = "mssql")]

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::mssql_server::MssqlServer;

use selene_core::driver::driver_for;
use selene_core::{
    plan_moves, AuthMethod, BackupOptions, CancelToken, CellValue, Column, Connection, CoreError,
    CsvImportOptions, CsvRowSource, DestColumn, DriverId, ExecOptions, ExportFormat, Exporter,
    Flow, ImportTarget, LogicalType, NewColumn, RestoreOptions, RowSink, TemporalKind, TlsConfig,
};
use selene_core::{ConnectionSpec, Secret};

/// The SA password the `mssql_server` module configures by default. It already
/// satisfies SQL Server's complexity policy (upper, lower, digit, symbol).
const SA_PASSWORD: &str = MssqlServer::DEFAULT_SA_PASSWORD;

/// A live test fixture: the connected `Connection` plus the running container.
///
/// The `ContainerAsync` guard MUST be held for the lifetime of the test —
/// dropping it stops and removes the container (and so kills the connection).
struct Fixture {
    conn: Box<dyn Connection>,
    /// The mapped host port — lets a test open a second, independent connection
    /// to the same server (e.g. to hold a database "in use").
    port: u16,
    // Kept alive to keep the container running; never read directly.
    _container: ContainerAsync<MssqlServer>,
}

/// Start a fresh SQL Server container and open a `selene-core` connection to it
/// as `sa`. Returns the connection and the container guard.
///
/// The spec uses `encrypt = true` + `trust_server_certificate = true`: the
/// container presents a self-signed certificate, so trusting it is required and
/// also exercises that TLS path of the driver/config builder.
async fn start_mssql() -> Fixture {
    let container = MssqlServer::default()
        .with_accept_eula()
        .start()
        .await
        .expect("start SQL Server container");

    let port = container
        .get_host_port_ipv4(1433)
        .await
        .expect("map container port 1433 to a host port");

    let spec = ConnectionSpec {
        id: "it-mssql".to_string(),
        name: "integration".to_string(),
        driver: DriverId::Mssql,
        host: "127.0.0.1".to_string(),
        port: Some(port),
        instance: None,
        database: None,
        auth: AuthMethod::SqlLogin {
            username: "sa".to_string(),
        },
        tls: TlsConfig {
            encrypt: true,
            trust_server_certificate: true,
        },
        read_only: false,
    };

    let driver = driver_for(DriverId::Mssql).expect("mssql driver compiled in");
    let conn = driver
        .connect(&spec, &Secret::new(SA_PASSWORD))
        .await
        .expect("connect to SQL Server");

    Fixture {
        conn,
        port,
        _container: container,
    }
}

/// Open an additional `selene-core` connection (as `sa`) to an already-running
/// container, given its mapped port. Used to simulate a second client that
/// holds a database "in use".
async fn connect_mssql(port: u16) -> Box<dyn Connection> {
    let driver = driver_for(DriverId::Mssql).expect("mssql driver compiled in");
    driver
        .connect(&spec_for(port), &Secret::new(SA_PASSWORD))
        .await
        .expect("open a second connection to SQL Server")
}

/// Build the `ConnectionSpec` for `test_connection` (no live `connect`), reusing
/// the running container's mapped port.
fn spec_for(port: u16) -> ConnectionSpec {
    ConnectionSpec {
        id: "it-mssql".to_string(),
        name: "integration".to_string(),
        driver: DriverId::Mssql,
        host: "127.0.0.1".to_string(),
        port: Some(port),
        instance: None,
        database: None,
        auth: AuthMethod::SqlLogin {
            username: "sa".to_string(),
        },
        tls: TlsConfig {
            encrypt: true,
            trust_server_certificate: true,
        },
        read_only: false,
    }
}

/// What one result set looked like to a [`CollectingSink`].
#[derive(Clone, Debug, Default)]
struct CapturedSet {
    /// Column metadata from `on_meta`.
    columns: Vec<Column>,
    /// All rows across every `on_rows` batch, flattened in arrival order.
    rows: Vec<Vec<CellValue>>,
    /// How many `on_rows` calls (batches) were delivered for this set.
    batch_count: usize,
    /// How many times `on_set_end` fired for this set.
    set_end_count: usize,
    /// The `affected` argument from each `on_set_end`.
    affected: Vec<Option<u64>>,
}

/// A [`RowSink`] that records everything per `set_index`: columns, flattened
/// rows, batch counts, and set-end calls — enough for tests to assert metadata,
/// row contents, and set boundaries.
///
/// State lives behind an `Arc<Mutex<..>>` so a test can inspect it after the
/// borrow held by `execute` ends, and so a cancellation hook can observe it.
#[derive(Clone, Default)]
struct CollectingSink {
    sets: Arc<Mutex<Vec<CapturedSet>>>,
    /// Optional cancel hook: when set, `cancel()` is invoked after the Nth
    /// `on_rows` batch (counted across all sets).
    cancel_after_batches: Option<usize>,
    cancel_token: Option<CancelToken>,
    /// Running count of `on_rows` calls observed (across all sets).
    batches_seen: Arc<Mutex<usize>>,
}

impl CollectingSink {
    fn new() -> Self {
        Self::default()
    }

    /// Configure the sink to call `token.cancel()` immediately after the
    /// `after`-th `on_rows` batch (1 = after the very first batch).
    fn cancel_after(after: usize, token: CancelToken) -> Self {
        Self {
            cancel_after_batches: Some(after),
            cancel_token: Some(token),
            ..Self::default()
        }
    }

    /// Snapshot of the captured sets.
    fn sets(&self) -> Vec<CapturedSet> {
        self.sets.lock().unwrap().clone()
    }

    /// Total rows captured across all sets.
    fn total_rows(&self) -> usize {
        self.sets.lock().unwrap().iter().map(|s| s.rows.len()).sum()
    }

    /// Ensure a `CapturedSet` exists at `set_index`, growing the vec as needed.
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

        // Cooperative-cancellation hook: trip the token after the configured
        // batch. We return `Continue` so the *driver* is the one that observes
        // the cancel at the next loop boundary (which is exactly the behaviour
        // under test). Returning `Stop` here would instead exercise the
        // sink-stop path, not cancellation.
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

/// Run `sql` with `opts`, collecting into a fresh [`CollectingSink`]. Returns
/// the outcome and the sink so tests can assert on both.
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

/// Execute a statement for its side effects (DDL/DML), discarding rows. Panics
/// on error so setup failures surface immediately.
async fn exec_ok(conn: &mut dyn Connection, sql: &str) {
    let mut sink = CollectingSink::new();
    let cancel = CancelToken::new();
    conn.execute(sql, &ExecOptions::default(), &mut sink, &cancel)
        .await
        .unwrap_or_else(|e| panic!("statement failed: {sql}\n  error: {e}"));
}

// ---------------------------------------------------------------------------
// 1. connect + test_connection
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn connect_and_test_connection_reports_server_version() {
    let fixture = start_mssql().await;
    let port = fixture._container.get_host_port_ipv4(1433).await.unwrap();

    let driver = driver_for(DriverId::Mssql).unwrap();
    let report = driver
        .test_connection(&spec_for(port), &Secret::new(SA_PASSWORD))
        .await
        .expect("test_connection succeeds");

    let version = report
        .server_version
        .expect("server_version should be Some");
    assert!(
        !version.is_empty(),
        "server_version should be non-empty, got {version:?}"
    );
    // @@VERSION on SQL Server always starts with "Microsoft SQL Server".
    assert!(
        version.contains("Microsoft SQL Server"),
        "unexpected @@VERSION banner: {version}"
    );
}

// ---------------------------------------------------------------------------
// 2. typed scalar SELECT
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn typed_scalar_select_maps_every_cell() {
    let mut fixture = start_mssql().await;

    let sql = "SELECT \
        CAST(1 AS int) AS i, \
        CAST(12345.6789 AS decimal(18,4)) AS d, \
        CAST(N'héllo' AS nvarchar(50)) AS s, \
        CAST(1 AS bit) AS b, \
        CAST(NULL AS int) AS n, \
        SYSDATETIME() AS dt, \
        NEWID() AS g, \
        CAST(0x1234 AS varbinary(8)) AS bin";

    let (outcome, sink) = run(fixture.conn.as_mut(), sql, &ExecOptions::default()).await;

    assert_eq!(outcome.result_sets, 1, "single result set");
    assert_eq!(outcome.total_rows, 1, "single row");
    assert!(!outcome.truncated);

    let sets = sink.sets();
    assert_eq!(sets.len(), 1);
    let set = &sets[0];

    // Column count and names (in order).
    let names: Vec<&str> = set.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["i", "d", "s", "b", "n", "dt", "g", "bin"]);

    assert_eq!(set.rows.len(), 1);
    let row = &set.rows[0];
    assert_eq!(row.len(), 8, "eight cells");

    // i: int -> I64(1)
    assert_eq!(row[0], CellValue::I64(1));

    // d: decimal(18,4) -> exact string, precision (trailing zeros) preserved.
    assert_eq!(
        row[1],
        CellValue::Decimal("12345.6789".to_string()),
        "decimal must round-trip as an EXACT string"
    );

    // s: nvarchar -> String, unicode preserved.
    assert_eq!(row[2], CellValue::String("héllo".to_string()));

    // b: bit -> Bool(true)
    assert_eq!(row[3], CellValue::Bool(true));

    // n: CAST(NULL AS int) -> Null
    assert_eq!(row[4], CellValue::Null);

    // dt: SYSDATETIME() -> DateTime with DateTime kind and a non-empty ISO.
    match &row[5] {
        CellValue::DateTime { iso, kind } => {
            assert_eq!(*kind, TemporalKind::DateTime, "datetime2 -> DateTime kind");
            assert!(!iso.is_empty(), "datetime ISO must be non-empty");
            // ISO-8601 with a 'T' date/time separator and a leading 4-digit
            // year (e.g. 2026-06-15T12:34:56.789...). Shape, not a literal date,
            // since this reflects the container's wall clock.
            let (date, time) = iso
                .split_once('T')
                .unwrap_or_else(|| panic!("datetime ISO must contain a 'T': {iso}"));
            let year = &date[..4];
            assert!(
                year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()),
                "datetime ISO must start with a 4-digit year: {iso}"
            );
            assert!(
                !time.is_empty(),
                "datetime ISO must have a time part: {iso}"
            );
        }
        other => panic!("dt should be DateTime, got {other:?}"),
    }

    // g: NEWID() -> Uuid, canonical 36-char form.
    match &row[6] {
        CellValue::Uuid(u) => {
            assert_eq!(u.len(), 36, "canonical uuid has 36 chars: {u}");
            assert_eq!(u.matches('-').count(), 4, "uuid has four hyphens: {u}");
        }
        other => panic!("g should be Uuid, got {other:?}"),
    }

    // bin: varbinary(0x1234) -> Bytes([0x12, 0x34])
    assert_eq!(row[7], CellValue::Bytes(vec![0x12, 0x34]));
}

// ---------------------------------------------------------------------------
// 3. many rows + batching/order
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn many_rows_are_batched_and_ordered() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();

    // A temp table populated with 1500 sequential ids via a numbers CTE.
    exec_ok(conn, "CREATE TABLE #nums (id int NOT NULL PRIMARY KEY)").await;
    exec_ok(
        conn,
        "WITH n AS ( \
            SELECT 0 AS id \
            UNION ALL \
            SELECT id + 1 FROM n WHERE id < 1499 \
         ) \
         INSERT INTO #nums (id) SELECT id FROM n OPTION (MAXRECURSION 0)",
    )
    .await;

    let opts = ExecOptions {
        max_rows: None,
        batch_size: 100,
    };
    let (outcome, sink) = run(conn, "SELECT id FROM #nums ORDER BY id", &opts).await;

    assert_eq!(outcome.total_rows, 1500, "all rows delivered");
    assert_eq!(outcome.result_sets, 1);
    assert!(!outcome.truncated);

    let sets = sink.sets();
    let set = &sets[0];
    assert_eq!(set.rows.len(), 1500);
    assert!(
        set.batch_count > 1,
        "expected multiple on_rows batches at batch_size=100, got {}",
        set.batch_count
    );
    // With 1500 rows at batch_size 100 we expect ~15 batches.
    assert!(
        set.batch_count >= 15,
        "expected >= 15 batches, got {}",
        set.batch_count
    );
    assert_eq!(set.set_end_count, 1, "exactly one on_set_end for the set");

    // Ids must be exactly 0..1500 in order.
    for (expected, row) in set.rows.iter().enumerate() {
        assert_eq!(
            row[0],
            CellValue::I64(expected as i64),
            "row {expected} out of order"
        );
    }
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn dml_reports_rows_affected_as_columnless_result_set() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();

    exec_ok(
        conn,
        "CREATE TABLE #affected (id int NOT NULL PRIMARY KEY, done bit NOT NULL)",
    )
    .await;
    exec_ok(
        conn,
        "INSERT INTO #affected (id, done) VALUES (1, 0), (2, 0), (3, 0)",
    )
    .await;

    let (outcome, sink) = run(
        conn,
        "UPDATE #affected SET done = 1 WHERE id IN (1, 2)",
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.result_sets, 1);
    assert_eq!(outcome.total_rows, 0);
    assert!(!outcome.truncated);
    assert!(!outcome.rolled_back, "plain DML is not a rollback dry-run");

    let sets = sink.sets();
    let set = &sets[0];
    assert!(
        set.columns.is_empty(),
        "DML count set should have no columns"
    );
    assert!(set.rows.is_empty(), "DML count set should have no row data");
    assert_eq!(set.set_end_count, 1);
    assert_eq!(set.affected, vec![Some(2)]);
}

/// The reported workflow: a leading `USE <db>` followed by a rollback-wrapped
/// multi-INSERT batch, with `BEGIN TRANSACTION` on its own line (no `;`, so the
/// split glues it to the next statement) and a `SET @id = SCOPE_IDENTITY()`
/// between the inserts. The driver must:
///   * actually switch the connection's database — the `USE` runs on the
///     persistent batch path, not inside the affected-count `sp_executesql`
///     call (where it would not persist), and
///   * report each row-affecting statement's count as a column-less set — the
///     two INSERTs and the `SET @id = (SELECT ...)` assignment each touch one
///     row — while trimming the `BEGIN TRAN` / `ROLLBACK` wrapper's phantom
///     zero-count sets.
/// The transaction is rolled back, so the table is left empty.
#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn use_prefixed_rollback_dml_switches_db_and_reports_counts() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();

    // A separate database with a table, so the leading USE is a real switch.
    exec_ok(conn, "CREATE DATABASE selene_use_it").await;
    exec_ok(
        conn,
        "USE selene_use_it; CREATE TABLE dbo.report \
         (id int IDENTITY PRIMARY KEY, name nvarchar(50) NOT NULL)",
    )
    .await;

    // Return to master so the USE in the batch under test is a genuine switch.
    exec_ok(conn, "USE master").await;
    assert_eq!(conn.current_database().await.unwrap(), "master");

    let sql = "USE selene_use_it;\n\
        BEGIN TRANSACTION\n\
        DECLARE @id INT;\n\
        INSERT INTO dbo.report (name) VALUES (N'first');\n\
        SET @id = (SELECT SCOPE_IDENTITY());\n\
        INSERT INTO dbo.report (name) SELECT name FROM dbo.report WHERE id = @id;\n\
        ROLLBACK";

    let (outcome, sink) = run(conn, sql, &ExecOptions::default()).await;

    // Three column-less affected-count sets, each touching exactly one row: the
    // two INSERTs plus the `SET @id = (SELECT SCOPE_IDENTITY())` assignment,
    // which SQL Server also reports as one row affected (SSMS prints three
    // "(1 row affected)" messages for this batch). The wrapper's genuine
    // zero-count sets (BEGIN TRAN / DECLARE / ROLLBACK) are still trimmed —
    // without trimming there would be many more.
    assert_eq!(
        outcome.result_sets, 3,
        "two INSERTs + the scalar SELECT assignment → three count sets"
    );
    assert_eq!(outcome.total_rows, 0);
    assert!(
        outcome.rolled_back,
        "rollback-wrapped dry-run must be flagged as rolled back"
    );
    let sets = sink.sets();
    assert_eq!(sets.len(), 3);
    for set in &sets {
        assert!(set.columns.is_empty(), "count set has no columns");
        assert!(set.rows.is_empty(), "count set has no row data");
        assert_eq!(set.affected, vec![Some(1)], "each set touches one row");
    }

    // The leading USE persisted: the connection is now in the target database.
    assert_eq!(
        conn.current_database().await.unwrap(),
        "selene_use_it",
        "leading USE must switch the connection's database context"
    );

    // The transaction was rolled back, so the dry-run inserts left no rows.
    let (_o, count_sink) = run(
        conn,
        "SELECT COUNT(*) AS n FROM dbo.report",
        &ExecOptions::default(),
    )
    .await;
    assert_eq!(
        count_sink.sets()[0].rows,
        vec![vec![CellValue::I64(0)]],
        "ROLLBACK must leave the table empty"
    );

    // Cleanup (cannot DROP the database we are connected to).
    exec_ok(conn, "USE master").await;
    exec_ok(conn, "DROP DATABASE selene_use_it").await;
}

// ---------------------------------------------------------------------------
// 4. max_rows truncation
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn max_rows_truncates_at_the_cap() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();

    exec_ok(conn, "CREATE TABLE #nums (id int NOT NULL PRIMARY KEY)").await;
    exec_ok(
        conn,
        "WITH n AS ( \
            SELECT 0 AS id UNION ALL SELECT id + 1 FROM n WHERE id < 1499 \
         ) \
         INSERT INTO #nums (id) SELECT id FROM n OPTION (MAXRECURSION 0)",
    )
    .await;

    let opts = ExecOptions {
        max_rows: Some(500),
        batch_size: 100,
    };
    let (outcome, sink) = run(conn, "SELECT id FROM #nums ORDER BY id", &opts).await;

    assert!(outcome.truncated, "hitting max_rows must set truncated");
    assert_eq!(
        outcome.total_rows, 500,
        "exactly max_rows delivered to the outcome"
    );
    assert_eq!(
        sink.total_rows(),
        500,
        "exactly 500 rows delivered to the sink"
    );
}

// ---------------------------------------------------------------------------
// 5. multiple result sets
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn multiple_result_sets_are_indexed_separately() {
    let mut fixture = start_mssql().await;

    let (outcome, sink) = run(
        fixture.conn.as_mut(),
        "SELECT 1 AS a; SELECT 2 AS b, 3 AS c",
        &ExecOptions::default(),
    )
    .await;

    assert_eq!(outcome.result_sets, 2, "two result sets");
    assert_eq!(outcome.total_rows, 2, "one row per set");

    let sets = sink.sets();
    assert_eq!(sets.len(), 2, "two captured sets at index 0 and 1");

    // Set 0: single column `a`.
    assert_eq!(
        sets[0]
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    assert_eq!(sets[0].rows, vec![vec![CellValue::I64(1)]]);
    assert_eq!(sets[0].set_end_count, 1);

    // Set 1: two columns `b`, `c`.
    assert_eq!(
        sets[1]
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["b", "c"]
    );
    assert_eq!(
        sets[1].rows,
        vec![vec![CellValue::I64(2), CellValue::I64(3)]]
    );
    assert_eq!(sets[1].set_end_count, 1);
}

// ---------------------------------------------------------------------------
// 6. cooperative cancellation mid-stream
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn cooperative_cancellation_stops_mid_stream() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();

    exec_ok(conn, "CREATE TABLE #nums (id int NOT NULL PRIMARY KEY)").await;
    exec_ok(
        conn,
        "WITH n AS ( \
            SELECT 0 AS id UNION ALL SELECT id + 1 FROM n WHERE id < 1499 \
         ) \
         INSERT INTO #nums (id) SELECT id FROM n OPTION (MAXRECURSION 0)",
    )
    .await;

    let cancel = CancelToken::new();
    // Cancel right after the FIRST on_rows batch. Cooperative cancel is observed
    // by the driver at the next batch boundary, so the execution ends early with
    // CoreError::Cancelled. A small batch_size guarantees the first batch is far
    // short of all 1500 rows.
    let mut sink = CollectingSink::cancel_after(1, cancel.clone());

    let opts = ExecOptions {
        max_rows: None,
        batch_size: 50,
    };
    let err = conn
        .execute(
            "SELECT id FROM #nums ORDER BY id",
            &opts,
            &mut sink,
            &cancel,
        )
        .await
        .expect_err("a cancelled execution returns Err");

    assert!(
        matches!(err, CoreError::Cancelled),
        "expected CoreError::Cancelled, got {err:?}"
    );

    let delivered = sink.total_rows();
    assert!(
        delivered > 0 && delivered < 1500,
        "cancellation should deliver some but not all rows; got {delivered}"
    );
}

// ---------------------------------------------------------------------------
// 7. introspection
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn introspection_lists_databases_schemas_tables_columns() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();

    // Build a database with a schema, a table (with a PK + typed columns), and a
    // view. GO is a sqlcmd batch separator (not T-SQL), so each statement is run
    // as its own batch; CREATE SCHEMA / CREATE VIEW must each be the first
    // statement in their batch.
    exec_ok(
        conn,
        "IF DB_ID('selene_it') IS NULL CREATE DATABASE selene_it",
    )
    .await;
    exec_ok(conn, "USE selene_it").await;
    exec_ok(conn, "CREATE SCHEMA app").await;
    exec_ok(
        conn,
        "CREATE TABLE app.customers ( \
            id int NOT NULL PRIMARY KEY, \
            name nvarchar(100) NOT NULL, \
            email nvarchar(255) NULL, \
            balance decimal(18,2) NULL \
         )",
    )
    .await;
    exec_ok(
        conn,
        "CREATE VIEW app.v_customers AS SELECT id, name FROM app.customers",
    )
    .await;

    // --- databases ---
    let dbs = conn.list_databases().await.expect("list_databases");
    let selene = dbs
        .iter()
        .find(|d| d.name == "selene_it")
        .expect("selene_it present in list_databases");
    assert!(
        !selene.is_system,
        "a user database is not a system database"
    );
    // System databases must be present and flagged as system.
    let master = dbs.iter().find(|d| d.name == "master");
    if let Some(m) = master {
        assert!(m.is_system, "master must be flagged is_system=true");
    }
    assert!(
        dbs.iter().filter(|d| d.is_system).count() >= 1,
        "at least one system database (master/tempdb/model/msdb) expected"
    );

    // --- schemas ---
    let schemas = conn.list_schemas("selene_it").await.expect("list_schemas");
    let schema_names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
    assert!(
        schema_names.contains(&"app"),
        "user schema 'app' expected, got {schema_names:?}"
    );
    assert!(
        !schema_names.contains(&"sys"),
        "system schema 'sys' must be excluded, got {schema_names:?}"
    );
    assert!(
        !schema_names.contains(&"INFORMATION_SCHEMA"),
        "INFORMATION_SCHEMA must be excluded, got {schema_names:?}"
    );

    // --- tables + views ---
    let tables = conn
        .list_tables("selene_it", "app")
        .await
        .expect("list_tables");
    let table = tables
        .iter()
        .find(|t| t.name == "customers")
        .expect("customers table present");
    assert_eq!(
        table.kind,
        selene_core::TableKind::Table,
        "customers is a base table"
    );
    let view = tables
        .iter()
        .find(|t| t.name == "v_customers")
        .expect("v_customers view present");
    assert_eq!(
        view.kind,
        selene_core::TableKind::View,
        "v_customers is a view"
    );

    // --- columns ---
    let cols = conn
        .list_columns("selene_it", "app", "customers")
        .await
        .expect("list_columns");
    let by_name = |n: &str| {
        cols.iter()
            .find(|c| c.name == n)
            .unwrap_or_else(|| panic!("column {n} missing; got {cols:?}"))
    };

    let id = by_name("id");
    assert_eq!(id.data_type, "int");
    assert!(!id.nullable, "id is NOT NULL");
    assert!(id.is_primary_key, "id is the primary key");

    let name = by_name("name");
    assert_eq!(name.data_type, "nvarchar");
    assert!(!name.nullable, "name is NOT NULL");
    assert!(!name.is_primary_key);

    let email = by_name("email");
    assert_eq!(email.data_type, "nvarchar");
    assert!(email.nullable, "email is NULL-able");
    assert!(!email.is_primary_key);

    let balance = by_name("balance");
    assert_eq!(balance.data_type, "decimal");
    assert!(balance.nullable);
    assert!(!balance.is_primary_key);

    // Exactly one PK column.
    assert_eq!(
        cols.iter().filter(|c| c.is_primary_key).count(),
        1,
        "exactly one primary-key column"
    );
}

// ---------------------------------------------------------------------------
// 7b. database management: rename + offline/online
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn rename_and_offline_online_round_trip() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();

    exec_ok(
        conn,
        "IF DB_ID('selene_mgmt') IS NULL CREATE DATABASE selene_mgmt",
    )
    .await;

    // Rename (clean path: no other sessions), then confirm the new name is
    // listed and the old one is gone.
    conn.rename_database("selene_mgmt", "selene_mgmt2", false)
        .await
        .expect("rename_database");
    let dbs = conn.list_databases().await.expect("list after rename");
    assert!(
        dbs.iter().any(|d| d.name == "selene_mgmt2"),
        "renamed database should be listed"
    );
    assert!(
        !dbs.iter().any(|d| d.name == "selene_mgmt"),
        "old database name should be gone"
    );

    // Take offline: it must still be listed (so the UI can bring it back),
    // now reporting state_desc = OFFLINE.
    conn.set_database_online("selene_mgmt2", false)
        .await
        .expect("set offline");
    let dbs = conn.list_databases().await.expect("list after offline");
    let offline = dbs
        .iter()
        .find(|d| d.name == "selene_mgmt2")
        .expect("offline database still listed");
    assert_eq!(offline.state_desc, "OFFLINE");

    // Bring it back online.
    conn.set_database_online("selene_mgmt2", true)
        .await
        .expect("set online");
    let dbs = conn.list_databases().await.expect("list after online");
    let online = dbs
        .iter()
        .find(|d| d.name == "selene_mgmt2")
        .expect("online database listed");
    assert_eq!(online.state_desc, "ONLINE");
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn rename_in_use_reports_then_force_succeeds() {
    let mut fixture = start_mssql().await;
    let port = fixture.port;

    exec_ok(
        fixture.conn.as_mut(),
        "IF DB_ID('selene_force') IS NULL CREATE DATABASE selene_force",
    )
    .await;

    // A second, independent connection that sits *inside* the target database,
    // so the clean rename can't acquire exclusive access.
    let mut holder = connect_mssql(port).await;
    exec_ok(holder.as_mut(), "USE selene_force").await;

    // Clean rename (force = false) must fail fast with DatabaseInUse rather than
    // blocking on the lock (the LOCK_TIMEOUT / exclusive-lock error).
    let err = fixture
        .conn
        .rename_database("selene_force", "selene_force2", false)
        .await
        .expect_err("clean rename must fail while the database is in use");
    assert!(
        matches!(err, CoreError::DatabaseInUse(_)),
        "expected DatabaseInUse, got {err:?}",
    );
    // The name must be unchanged after the failed clean attempt.
    let dbs = fixture
        .conn
        .list_databases()
        .await
        .expect("list after block");
    assert!(
        dbs.iter().any(|d| d.name == "selene_force"),
        "database keeps its original name after a blocked rename",
    );

    // Force rename (force = true) disconnects the holder (ROLLBACK IMMEDIATE)
    // and completes, leaving the database back in MULTI_USER under the new name.
    fixture
        .conn
        .rename_database("selene_force", "selene_force2", true)
        .await
        .expect("force rename should succeed");
    let dbs = fixture
        .conn
        .list_databases()
        .await
        .expect("list after force rename");
    let renamed = dbs
        .iter()
        .find(|d| d.name == "selene_force2")
        .expect("force-renamed database should be listed");
    assert_eq!(
        renamed.state_desc, "ONLINE",
        "MULTI_USER restored: the database is ONLINE, not stuck single-user",
    );
    assert!(
        !dbs.iter().any(|d| d.name == "selene_force"),
        "old name should be gone after the force rename",
    );

    // The holder's session was rolled back by the force rename; drop it.
    drop(holder);
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn create_and_drop_database_round_trip() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();

    // Create, then confirm it is listed and ONLINE.
    conn.create_database("selene_create")
        .await
        .expect("create_database");
    let dbs = conn.list_databases().await.expect("list after create");
    let created = dbs
        .iter()
        .find(|d| d.name == "selene_create")
        .expect("created database should be listed");
    assert_eq!(created.state_desc, "ONLINE");
    assert!(!created.is_system);

    // Drop, then confirm it is gone.
    conn.drop_database("selene_create")
        .await
        .expect("drop_database");
    let dbs = conn.list_databases().await.expect("list after drop");
    assert!(
        !dbs.iter().any(|d| d.name == "selene_create"),
        "dropped database should no longer be listed"
    );
}

// ---------------------------------------------------------------------------
// 8. export round-trip (CSV)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn csv_export_round_trip_against_real_driver() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();

    // Drive the real driver straight into the CSV exporter (the exporter IS a
    // RowSink). A handful of typed cells exercise int / nvarchar / decimal.
    let tmp = tempfile::NamedTempFile::new().expect("temp file");
    let mut exporter = Exporter::create(
        ExportFormat::Csv,
        tmp.path(),
        selene_core::CsvOptions {
            delimiter: b',',
            ..Default::default()
        },
    )
    .expect("create CSV exporter");

    let cancel = CancelToken::new();
    let sql = "SELECT CAST(id AS int) AS id, name, amount FROM (VALUES \
        (1, N'alpha', CAST(10.50 AS decimal(18,2))), \
        (2, N'beta',  CAST(20.25 AS decimal(18,2)))) AS v(id, name, amount) \
        ORDER BY id";

    let outcome = conn
        .execute(sql, &ExecOptions::default(), &mut exporter, &cancel)
        .await
        .expect("export query executes");
    assert_eq!(outcome.total_rows, 2);

    let summary = exporter.finish().expect("finish CSV export");
    assert_eq!(summary.rows_written, 2);

    // Read it back. CsvOptions defaults line_ending to CRLF (RFC-4180 / Excel),
    // so the writer terminates each record with \r\n. Nothing is quoted here (no
    // commas/quotes/newlines in the data); decimal keeps its exact text.
    let contents = std::fs::read_to_string(tmp.path()).expect("read CSV back");
    let expected = "id,name,amount\r\n1,alpha,10.50\r\n2,beta,20.25\r\n";
    assert_eq!(contents, expected, "CSV round-trip mismatch");
}

// ---------------------------------------------------------------------------
// 9. import round-trip (CSV → table)
// ---------------------------------------------------------------------------

/// Write `contents` to a temp `.csv` file, returning the handle (kept alive by
/// the caller so the file isn't deleted before it's read).
fn write_csv(contents: &str) -> tempfile::NamedTempFile {
    use std::io::Write as _;
    let mut f = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("temp csv");
    f.write_all(contents.as_bytes()).expect("write csv");
    f.flush().expect("flush csv");
    f
}

fn import_opts(atomic: bool) -> CsvImportOptions {
    CsvImportOptions {
        delimiter: b',',
        atomic,
        ..Default::default()
    }
}

/// Create a fresh database and `USE` it, returning its name.
async fn fresh_db(conn: &mut dyn Connection, name: &str) -> String {
    exec_ok(
        conn,
        &format!("IF DB_ID('{name}') IS NULL CREATE DATABASE {name}"),
    )
    .await;
    exec_ok(conn, &format!("USE {name}")).await;
    name.to_string()
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn import_creates_table_and_inserts_typed_rows() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();
    let db = fresh_db(conn, "selene_imp_new").await;

    let csv = write_csv("id,name,amount\n1,alpha,10.50\n2,beta,20.25\n");

    // Create the table, then stream the CSV into it.
    let columns = vec![
        NewColumn {
            name: "id".into(),
            sql_type: "INT".into(),
            nullable: true,
        },
        NewColumn {
            name: "name".into(),
            sql_type: "NVARCHAR(100)".into(),
            nullable: true,
        },
        NewColumn {
            name: "amount".into(),
            sql_type: "DECIMAL(18,2)".into(),
            nullable: true,
        },
    ];
    conn.create_table(Some(&db), "dbo", "imported", &columns, &CancelToken::new())
        .await
        .expect("create_table");

    let dest = vec![
        DestColumn {
            csv_index: Some(0),
            logical: LogicalType::Integer,
        },
        DestColumn {
            csv_index: Some(1),
            logical: LogicalType::Text,
        },
        DestColumn {
            csv_index: Some(2),
            logical: LogicalType::Decimal,
        },
    ];
    let mut source =
        CsvRowSource::open(csv.path(), &import_opts(true), dest, 500).expect("open csv source");
    let target = ImportTarget {
        database: Some(db.clone()),
        schema: "dbo".into(),
        table: "imported".into(),
        columns: vec!["id".into(), "name".into(), "amount".into()],
    };
    let inserted = conn
        .import_rows(&target, &mut source, true, 500, &CancelToken::new())
        .await
        .expect("import_rows");
    assert_eq!(inserted, 2);

    // Read it back and assert the typed values round-tripped exactly.
    let (_outcome, sink) = run(
        conn,
        &format!("SELECT id, name, amount FROM {db}.dbo.imported ORDER BY id"),
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

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn drop_table_clears_a_conflict_so_reimport_can_recreate() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();
    let db = fresh_db(conn, "selene_drop_tbl").await;

    let columns = vec![NewColumn {
        name: "id".into(),
        sql_type: "INT".into(),
        nullable: true,
    }];

    // First create succeeds.
    conn.create_table(Some(&db), "dbo", "imported", &columns, &CancelToken::new())
        .await
        .expect("first create_table");

    // Re-creating the same table fails with SQL Server error 2714 — the signal
    // the import modal keys on to offer "drop & retry".
    let err = conn
        .create_table(Some(&db), "dbo", "imported", &columns, &CancelToken::new())
        .await
        .expect_err("recreating an existing table must fail");
    assert!(
        err.to_string().contains("2714"),
        "expected SQL Server error 2714, got: {err}"
    );

    // Dropping clears the conflict, so the retry can recreate the table.
    conn.drop_table(Some(&db), "dbo", "imported", &CancelToken::new())
        .await
        .expect("drop_table");
    conn.create_table(Some(&db), "dbo", "imported", &columns, &CancelToken::new())
        .await
        .expect("create_table after drop");
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn import_into_existing_table_with_subset_mapping() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();
    let db = fresh_db(conn, "selene_imp_exist").await;
    exec_ok(
        conn,
        "CREATE TABLE dbo.people (id int NULL, full_name nvarchar(100) NULL, note nvarchar(50) NULL)",
    )
    .await;

    // CSV columns are in a different order and omit `note` entirely.
    let csv = write_csv("name,id\nAlice,1\nBob,2\n");
    let dest = vec![
        // target.columns order is [id, full_name]; map to CSV fields 1 and 0.
        DestColumn {
            csv_index: Some(1),
            logical: LogicalType::Integer,
        },
        DestColumn {
            csv_index: Some(0),
            logical: LogicalType::Text,
        },
    ];
    let mut source =
        CsvRowSource::open(csv.path(), &import_opts(true), dest, 500).expect("open csv source");
    let target = ImportTarget {
        database: Some(db.clone()),
        schema: "dbo".into(),
        table: "people".into(),
        columns: vec!["id".into(), "full_name".into()],
    };
    let inserted = conn
        .import_rows(&target, &mut source, true, 500, &CancelToken::new())
        .await
        .expect("import_rows");
    assert_eq!(inserted, 2);

    let (_outcome, sink) = run(
        conn,
        &format!("SELECT id, full_name, note FROM {db}.dbo.people ORDER BY id"),
        &ExecOptions::default(),
    )
    .await;
    let rows = sink.sets()[0].rows.clone();
    assert_eq!(
        rows,
        vec![
            vec![
                CellValue::I64(1),
                CellValue::String("Alice".into()),
                CellValue::Null
            ],
            vec![
                CellValue::I64(2),
                CellValue::String("Bob".into()),
                CellValue::Null
            ],
        ],
        "unmapped `note` column must be NULL",
    );
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn atomic_import_rolls_back_on_bad_row() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();
    let db = fresh_db(conn, "selene_imp_atomic").await;
    exec_ok(conn, "CREATE TABLE dbo.nums (n int NULL)").await;

    // Row 2 ("oops") cannot coerce to INT; atomic mode must abort + roll back.
    let csv = write_csv("n\n1\noops\n3\n");
    let dest = vec![DestColumn {
        csv_index: Some(0),
        logical: LogicalType::Integer,
    }];
    let mut source =
        CsvRowSource::open(csv.path(), &import_opts(true), dest, 500).expect("open csv source");
    let target = ImportTarget {
        database: Some(db.clone()),
        schema: "dbo".into(),
        table: "nums".into(),
        columns: vec!["n".into()],
    };
    let err = conn
        .import_rows(&target, &mut source, true, 500, &CancelToken::new())
        .await
        .expect_err("a bad row aborts an atomic import");
    assert!(matches!(err, CoreError::Import(_)), "got {err:?}");

    let (_outcome, sink) = run(
        conn,
        &format!("SELECT COUNT(*) AS n FROM {db}.dbo.nums"),
        &ExecOptions::default(),
    )
    .await;
    assert_eq!(
        sink.sets()[0].rows[0][0],
        CellValue::I64(0),
        "the transaction must have rolled back, leaving zero rows",
    );
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn skip_mode_imports_good_rows_and_reports_skips() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();
    let db = fresh_db(conn, "selene_imp_skip").await;
    exec_ok(conn, "CREATE TABLE dbo.nums2 (n int NULL)").await;

    let csv = write_csv("n\n1\noops\n3\n");
    let dest = vec![DestColumn {
        csv_index: Some(0),
        logical: LogicalType::Integer,
    }];
    let mut source =
        CsvRowSource::open(csv.path(), &import_opts(false), dest, 500).expect("open csv source");
    let target = ImportTarget {
        database: Some(db.clone()),
        schema: "dbo".into(),
        table: "nums2".into(),
        columns: vec!["n".into()],
    };
    let inserted = conn
        .import_rows(&target, &mut source, false, 500, &CancelToken::new())
        .await
        .expect("skip-mode import succeeds");
    assert_eq!(inserted, 2, "the two valid rows are inserted");
    assert_eq!(source.rows_skipped(), 1, "the one bad row is skipped");

    let (_outcome, sink) = run(
        conn,
        &format!("SELECT n FROM {db}.dbo.nums2 ORDER BY n"),
        &ExecOptions::default(),
    )
    .await;
    let rows = sink.sets()[0].rows.clone();
    assert_eq!(rows, vec![vec![CellValue::I64(1)], vec![CellValue::I64(3)]]);
}

// ---------------------------------------------------------------------------
// 12. backup + restore: back up one database and restore it OVER another
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn backup_then_restore_over_existing_database() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();

    // Source database with a known row.
    exec_ok(conn, "CREATE DATABASE selene_bak_src").await;
    exec_ok(
        conn,
        "USE selene_bak_src; \
         CREATE TABLE dbo.payload (id INT NOT NULL, tag NVARCHAR(20) NOT NULL); \
         INSERT INTO dbo.payload (id, tag) VALUES (42, N'from-source')",
    )
    .await;

    // A separate, existing target with *different* contents — this is the
    // database the backup will be laid over. CREATE DATABASE and a `USE` of the
    // new database cannot share one batch (the `USE` can't bind a database that
    // does not exist at compile time), so they are two statements.
    exec_ok(conn, "USE master; CREATE DATABASE selene_bak_tgt").await;
    exec_ok(
        conn,
        "USE selene_bak_tgt; \
         CREATE TABLE dbo.other (x INT NOT NULL); \
         INSERT INTO dbo.other (x) VALUES (7)",
    )
    .await;

    // Back up the source to a path the container's mssql process can write.
    let bak = "/var/opt/mssql/data/selene_src.bak";
    conn.backup_database(
        "selene_bak_src",
        bak,
        &BackupOptions {
            compression: false,
            checksum: true,
            verify_after: true,
        },
        &CancelToken::new(),
    )
    .await
    .expect("backup_database");

    // FILELISTONLY exposes the source's files: at least one data + one log.
    let backup_files = conn.restore_filelist(bak).await.expect("restore_filelist");
    assert!(
        backup_files.iter().any(|f| f.is_data()),
        "backup should contain a data file"
    );
    assert!(
        backup_files.iter().any(|f| f.is_log()),
        "backup should contain a log file"
    );

    // Plan MOVE relocations onto the target's current files, then restore.
    let target_files = conn
        .database_files("selene_bak_tgt")
        .await
        .expect("database_files");
    let default_dirs = conn.default_file_dirs().await.expect("default_file_dirs");
    let moves = plan_moves(
        &backup_files,
        &target_files,
        &default_dirs,
        "selene_bak_tgt",
    );
    conn.restore_database(
        "selene_bak_tgt",
        bak,
        &moves,
        &RestoreOptions { checksum: true },
        &CancelToken::new(),
    )
    .await
    .expect("restore_database");

    // The target now holds the *source's* schema and data, under its own name.
    let (_outcome, sink) = run(
        conn,
        "SELECT id, tag FROM selene_bak_tgt.dbo.payload ORDER BY id",
        &ExecOptions::default(),
    )
    .await;
    let rows = sink.sets()[0].rows.clone();
    assert_eq!(
        rows,
        vec![vec![
            CellValue::I64(42),
            CellValue::String("from-source".into()),
        ]],
        "restored target should contain the source backup's row"
    );

    // It is online and usable after the restore (multi-user restored).
    let dbs = conn.list_databases().await.expect("list after restore");
    let tgt = dbs
        .iter()
        .find(|d| d.name == "selene_bak_tgt")
        .expect("target listed");
    assert_eq!(tgt.state_desc, "ONLINE");
}

// ---------------------------------------------------------------------------
// 13. server filesystem browse (default backup dir + xp_dirtree listing)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn server_default_backup_dir_and_directory_listing() {
    let mut fixture = start_mssql().await;
    let conn = fixture.conn.as_mut();

    // The instance reports a default backup/data path (Linux: /var/opt/mssql/...).
    let dir = conn.default_backup_dir().await.expect("default_backup_dir");
    assert!(
        dir.contains("/var/opt/mssql"),
        "default dir should be under the mssql data root, got {dir:?}"
    );

    // Browsing /var/opt/mssql lists its children, including the `data` subdir.
    let entries = conn
        .list_server_dir("/var/opt/mssql")
        .await
        .expect("list_server_dir");
    let data = entries
        .iter()
        .find(|e| e.name == "data")
        .expect("`data` directory should be listed under /var/opt/mssql");
    assert!(data.is_dir, "`data` should be reported as a directory");

    // A non-existent path lists empty rather than erroring.
    let empty = conn
        .list_server_dir("/no/such/path/here")
        .await
        .expect("listing a missing dir is not an error");
    assert!(empty.is_empty(), "missing directory yields no entries");
}
