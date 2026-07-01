//! Integration tests for the MongoDB driver against a **real** MongoDB server.
//!
//! These spin up the official `mongo` image in Docker via [`testcontainers`]
//! (the `mongo` module of `testcontainers-modules`) and exercise `selene-core`'s
//! public MongoDB driver API for **M1**: `test_connection`, `connect` + `ping`,
//! `list_databases`, and that `execute` is (deliberately) still unsupported.
//! Query execution and introspection-by-sampling land in later PRs.
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

#![cfg(feature = "mongodb")]

use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::mongo::Mongo;

use selene_core::driver::driver_for;
use selene_core::{
    AuthMethod, CancelToken, Connection, CoreError, DriverId, ExecOptions, Flow, RowSink, Secret,
    TlsConfig,
};
use selene_core::{CellValue, Column, ConnectionSpec};

/// A live test fixture: the connected `Connection` plus the running container.
///
/// The `ContainerAsync` guard MUST be held for the lifetime of the test —
/// dropping it stops and removes the container (and so kills the connection).
struct Fixture {
    conn: Box<dyn Connection>,
    // Kept alive to keep the container running; never read directly.
    _container: ContainerAsync<Mongo>,
}

/// Build a `ConnectionSpec` for the mapped host port. The default `mongo` image
/// has authentication disabled, so `AuthMethod::None` (anonymous) is correct.
fn spec_for(port: u16) -> ConnectionSpec {
    ConnectionSpec {
        id: "it-mongodb".to_string(),
        name: "integration".to_string(),
        driver: DriverId::Mongodb,
        host: "127.0.0.1".to_string(),
        port: Some(port),
        instance: None,
        uri: None,
        database: None,
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

/// A no-op [`RowSink`]: `execute` is expected to fail before emitting anything.
struct NullSink;

#[async_trait::async_trait]
impl RowSink for NullSink {
    async fn on_meta(&mut self, _set_index: usize, _columns: Vec<Column>) -> Flow {
        Flow::Continue
    }
    async fn on_rows(&mut self, _set_index: usize, _rows: Vec<Vec<CellValue>>) -> Flow {
        Flow::Continue
    }
    async fn on_set_end(&mut self, _set_index: usize, _affected: Option<u64>) -> Flow {
        Flow::Continue
    }
}

// ---------------------------------------------------------------------------
// 1. test_connection reports a version banner
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
    // The mongo image is a 5.x/6.x server — a dotted numeric banner.
    assert!(
        version.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "unexpected mongodb version banner: {version}"
    );
}

// ---------------------------------------------------------------------------
// 2. connect + ping succeed
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn connect_and_ping_succeed() {
    let (mut fixture, _port) = start_mongodb().await;
    fixture.conn.ping().await.expect("ping succeeds");
}

// ---------------------------------------------------------------------------
// 3. list_databases includes the system databases, flagged is_system
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn list_databases_flags_system_databases() {
    let (mut fixture, _port) = start_mongodb().await;
    let dbs = fixture.conn.list_databases().await.expect("list_databases");

    // `admin` and `local` are always present on a running standalone server
    // (`config` only materialises in a sharded/replica-set deployment, so we do
    // not require it here). Each present system database must be flagged
    // `is_system` and reported ONLINE.
    for expected in ["admin", "local"] {
        let db = dbs
            .iter()
            .find(|d| d.name == expected)
            .unwrap_or_else(|| panic!("system database {expected} missing; got {dbs:?}"));
        assert!(db.is_system, "{expected} must be flagged is_system");
        assert_eq!(db.state_desc, "ONLINE");
    }

    // Every database's system flag must match the known internal-name set —
    // exercises the classification in both directions.
    for db in &dbs {
        let known_system = ["admin", "local", "config"].contains(&db.name.as_str());
        assert_eq!(
            db.is_system, known_system,
            "is_system misclassified for {}",
            db.name
        );
    }
}

// ---------------------------------------------------------------------------
// 4. execute is not yet supported (queries land in a later PR)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker"]
async fn execute_is_unsupported_for_now() {
    let (mut fixture, _port) = start_mongodb().await;
    let mut sink = NullSink;
    let cancel = CancelToken::new();
    let err = fixture
        .conn
        .execute("{ find: 'x' }", &ExecOptions::default(), &mut sink, &cancel)
        .await
        .expect_err("query execution is not implemented in M1");
    assert!(
        matches!(err, CoreError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}
