//! "Run on multiple targets" — run one SQL batch across many databases on many
//! servers, driven by Selene's saved connections.
//!
//! Three command surfaces back the feature:
//!
//! - [`multi_target_resolve`] runs a *filter query* against each selected
//!   connection's `master` and returns the database names it matched (column 0),
//!   so the UI can preview exactly which databases a run will touch. Per-server
//!   errors are captured, not fatal, so one unreachable server doesn't sink the
//!   whole preview.
//! - [`multi_target_run`] is the streaming workhorse. Given an explicit plan
//!   (`targets`: connection + database list) it opens one transient connection
//!   per server, runs each server's databases sequentially (`USE` → execute),
//!   and runs servers in parallel up to `max_parallel`. In `execute` mode rows
//!   are discarded (side-effects only); in `results` mode rows are aggregated —
//!   each prefixed with `_server`/`_database` — and streamed to the grid. It
//!   mirrors [`query_run`](super::query::query_run): returns a cancellation
//!   handle immediately and streams [`MultiEvent`]s over a `Channel`.
//! - [`export_result_set`] writes an already-collected result set to disk via
//!   the core [`Exporter`], backing the "Save CSV" button without re-running the
//!   queries.
//!
//! ## Logging discipline
//! Like every command here, this never logs SQL text, row/cell data, or secrets
//! above `DEBUG`/`TRACE`. Server/database names and aggregate counts are safe.
//!
//! ## Cancellation
//! Cooperative, reusing the shared [`CancelToken`] registry in
//! [`AppState::running`](crate::state::AppState::running): [`multi_target_cancel`]
//! flips the token; the task checks it between databases and the driver checks
//! it between row batches.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, Runtime, State};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use selene_core::{
    cell_to_text, classify, driver_for, CancelToken, CellValue, Column, ExecOptions, ExportFormat,
    ExportSummary, Exporter, Flow, GuardLevel, LogicalType, RowSink,
};

use crate::commands::export::CsvExportOptions;
use crate::commands::MultiEvent;
use crate::error::IpcError;
use crate::state::{new_id, AppState};

/// Rows buffered per batch before flushing; also the cancellation granularity.
const BATCH_SIZE: usize = 500;

/// Default per-database row cap in `results` mode (mirrors `query_run`). The cap
/// is **per database**, so an aggregate across N databases can be up to N× this.
const DEFAULT_MAX_ROWS: u64 = 50_000;

/// Generous cap on the filter query in [`multi_target_resolve`]; database lists
/// are small, but this stops a pathological filter query from running away.
const RESOLVE_MAX_ROWS: u64 = 100_000;

// --- wire types -----------------------------------------------------------

/// One run target: a saved connection plus the explicit databases to run on.
/// The UI resolves/selects these before calling [`multi_target_run`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiTarget {
    pub connection_id: String,
    pub databases: Vec<String>,
}

/// One entry of [`multi_target_resolve`]: the databases a filter query matched
/// on a connection, or the (sanitized) error that connection produced.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedTarget {
    pub connection_id: String,
    /// The connection's display name (used as the `_server` label).
    pub server: String,
    pub databases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What [`multi_target_run`] does on each target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MultiMode {
    /// Run for side-effects; rows are discarded.
    Execute,
    /// Aggregate rows into one result set (prefixed with `_server`/`_database`).
    Results,
}

/// Returned by [`multi_target_run`]: the id used to [`multi_target_cancel`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiRunHandle {
    pub run_id: String,
}

// --- sinks -----------------------------------------------------------------

/// Synthesize a non-null text column for the `_server`/`_database` prefix.
fn ident_col(name: &str, ordinal: usize) -> Column {
    Column {
        name: name.to_string(),
        ordinal,
        db_type: "nvarchar".to_string(),
        logical: LogicalType::Text,
        nullable: Some(false),
    }
}

/// Build the unified result columns: `_server`, `_database`, then the target's
/// own columns (re-ordinaled to follow the two prefix columns).
fn unify_columns(data_columns: Vec<Column>) -> Vec<Column> {
    let mut unified = Vec::with_capacity(data_columns.len() + 2);
    unified.push(ident_col("_server", 0));
    unified.push(ident_col("_database", 1));
    for (i, col) in data_columns.into_iter().enumerate() {
        unified.push(Column {
            ordinal: i + 2,
            ..col
        });
    }
    unified
}

/// Align one data row to `ref_cols` columns (pad with `NULL` / truncate extras)
/// and prepend the `_server`/`_database` cells, so every aggregated row matches
/// the unified column set regardless of small shape differences across targets.
fn prefixed_row(
    mut row: Vec<CellValue>,
    ref_cols: usize,
    server: &str,
    database: &str,
) -> Vec<CellValue> {
    if row.len() > ref_cols {
        row.truncate(ref_cols);
    } else {
        while row.len() < ref_cols {
            row.push(CellValue::Null);
        }
    }
    let mut out = Vec::with_capacity(ref_cols + 2);
    out.push(CellValue::String(server.to_string()));
    out.push(CellValue::String(database.to_string()));
    out.extend(row);
    out
}

/// A [`RowSink`] that grabs column 0 of every row as text — used to read a
/// filter query's database-name list.
#[derive(Default)]
struct CollectColumnSink {
    values: Vec<String>,
}

#[async_trait]
impl RowSink for CollectColumnSink {
    async fn on_meta(&mut self, _set_index: usize, _columns: Vec<Column>) -> Flow {
        Flow::Continue
    }

    async fn on_rows(&mut self, set_index: usize, rows: Vec<Vec<CellValue>>) -> Flow {
        if set_index == 0 {
            for row in rows {
                if let Some(first) = row.first() {
                    self.values.push(cell_to_text(first));
                }
            }
        }
        Flow::Continue
    }

    async fn on_set_end(&mut self, _set_index: usize, _affected: Option<u64>) -> Flow {
        Flow::Continue
    }
}

/// A sink for `execute` mode: statements run for their side-effects, rows are
/// discarded batch-by-batch (memory stays bounded), and the rows-affected
/// counts each DML statement reports are summed. It never signals `Stop`, so
/// every statement in a multi-statement batch runs.
#[derive(Default)]
struct CountSink {
    /// Sum of `affected` across every set that reported one.
    affected: u64,
    /// Whether any set reported an affected count — i.e. a DML statement ran.
    /// Lets the UI tell "a DML affected 0 rows" (worth flagging) apart from
    /// "no row-affecting statement at all" (DDL / SELECT — nothing to flag).
    had_affected: bool,
}

#[async_trait]
impl RowSink for CountSink {
    async fn on_meta(&mut self, _set_index: usize, _columns: Vec<Column>) -> Flow {
        Flow::Continue
    }
    async fn on_rows(&mut self, _set_index: usize, _rows: Vec<Vec<CellValue>>) -> Flow {
        Flow::Continue
    }
    async fn on_set_end(&mut self, _set_index: usize, affected: Option<u64>) -> Flow {
        if let Some(a) = affected {
            self.affected += a;
            self.had_affected = true;
        }
        Flow::Continue
    }
}

/// Shared reference for the aggregated result columns across parallel targets.
/// `None` until the first target's first set is seen; then `Some(ref_cols)`,
/// the data-column count (excluding the two prefix columns) every later target
/// is aligned to.
struct MetaState {
    ref_cols: Mutex<Option<usize>>,
}

/// A [`RowSink`] (one per target, `results` mode) that prepends `_server` and
/// `_database` cells to each row and forwards [`MultiEvent::Rows`]. The first
/// sink to see metadata emits the unified [`MultiEvent::Meta`]; later targets
/// align their rows to that column count (pad with `NULL` / truncate extras),
/// matching the documented "same query shape across databases" assumption.
struct AggSink {
    channel: Channel<MultiEvent>,
    server: String,
    database: String,
    shared: Arc<MetaState>,
    rows_total: Arc<AtomicU64>,
    /// Rows this target contributed (returned to the caller for `TargetDone`).
    rows_for_target: u64,
    /// Reference data-column count, learned on the first `on_meta`.
    ref_cols: usize,
}

impl AggSink {
    fn new(
        channel: Channel<MultiEvent>,
        server: String,
        database: String,
        shared: Arc<MetaState>,
        rows_total: Arc<AtomicU64>,
    ) -> Self {
        Self {
            channel,
            server,
            database,
            shared,
            rows_total,
            rows_for_target: 0,
            ref_cols: 0,
        }
    }
}

#[async_trait]
impl RowSink for AggSink {
    async fn on_meta(&mut self, set_index: usize, columns: Vec<Column>) -> Flow {
        if set_index != 0 {
            return Flow::Continue;
        }
        let mut guard = self.shared.ref_cols.lock().expect("meta mutex poisoned");
        match *guard {
            Some(ref_cols) => {
                self.ref_cols = ref_cols;
                Flow::Continue
            }
            None => {
                let ref_cols = columns.len();
                let unified = unify_columns(columns);
                *guard = Some(ref_cols);
                self.ref_cols = ref_cols;
                drop(guard);
                if self
                    .channel
                    .send(MultiEvent::Meta { columns: unified })
                    .is_err()
                {
                    return Flow::Stop;
                }
                Flow::Continue
            }
        }
    }

    async fn on_rows(&mut self, set_index: usize, rows: Vec<Vec<CellValue>>) -> Flow {
        if set_index != 0 {
            return Flow::Continue;
        }
        let m = self.ref_cols;
        let mut batch = Vec::with_capacity(rows.len());
        for row in rows {
            batch.push(prefixed_row(row, m, &self.server, &self.database));
        }
        let n = batch.len() as u64;
        self.rows_for_target += n;
        self.rows_total.fetch_add(n, Ordering::SeqCst);
        if self.channel.send(MultiEvent::Rows { rows: batch }).is_err() {
            return Flow::Stop;
        }
        Flow::Continue
    }

    async fn on_set_end(&mut self, _set_index: usize, _affected: Option<u64>) -> Flow {
        Flow::Continue
    }
}

// --- resolve ---------------------------------------------------------------

/// Run `filter_sql` against each connection and return the database names it
/// yields (column 0). Used for the run preview and as the plan source for
/// "generate script". Per-connection errors are captured into the result rather
/// than failing the whole call.
#[tauri::command]
pub async fn multi_target_resolve(
    state: State<'_, AppState>,
    connection_ids: Vec<String>,
    filter_sql: String,
) -> Result<Vec<ResolvedTarget>, IpcError> {
    let mut out = Vec::with_capacity(connection_ids.len());

    for connection_id in connection_ids {
        // Resolve the spec; an unknown connection is reported, not fatal.
        let spec = match state.store.get(&connection_id) {
            Ok(Some(spec)) => spec,
            Ok(None) => {
                out.push(ResolvedTarget {
                    connection_id: connection_id.clone(),
                    server: connection_id,
                    databases: Vec::new(),
                    error: Some("no saved connection with this id".to_string()),
                });
                continue;
            }
            Err(err) => {
                out.push(ResolvedTarget {
                    connection_id: connection_id.clone(),
                    server: connection_id,
                    databases: Vec::new(),
                    error: Some(IpcError::from(err).message),
                });
                continue;
            }
        };
        let server = spec.name.clone();

        let secret = match state.cached_secret(&connection_id) {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                out.push(ResolvedTarget {
                    connection_id,
                    server,
                    databases: Vec::new(),
                    error: Some("no stored password — connect to it once first".to_string()),
                });
                continue;
            }
            Err(err) => {
                out.push(ResolvedTarget {
                    connection_id,
                    server,
                    databases: Vec::new(),
                    error: Some(IpcError::from(err).message),
                });
                continue;
            }
        };

        let conn = match driver_for(spec.driver) {
            Ok(driver) => driver.connect(&spec, &secret).await,
            Err(err) => Err(err),
        };
        let mut conn = match conn {
            Ok(conn) => conn,
            Err(err) => {
                out.push(ResolvedTarget {
                    connection_id,
                    server,
                    databases: Vec::new(),
                    error: Some(IpcError::from(err).message),
                });
                continue;
            }
        };

        let opts = ExecOptions {
            max_rows: Some(RESOLVE_MAX_ROWS),
            batch_size: BATCH_SIZE,
        };
        let mut sink = CollectColumnSink::default();
        let cancel = CancelToken::new();
        match conn.execute(&filter_sql, &opts, &mut sink, &cancel).await {
            Ok(_) => out.push(ResolvedTarget {
                connection_id,
                server,
                databases: sink.values,
                error: None,
            }),
            Err(err) => out.push(ResolvedTarget {
                connection_id,
                server,
                databases: Vec::new(),
                error: Some(IpcError::from(err).message),
            }),
        }
    }

    Ok(out)
}

// --- run -------------------------------------------------------------------

/// Run `sql` across every (server, database) in `targets`, streaming progress
/// (and, in `results` mode, aggregated rows) over `on_event`. Returns a
/// [`MultiRunHandle`] immediately; the work runs in a detached task.
// A Tauri command surface: each argument maps to a JS IPC field.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn multi_target_run<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    targets: Vec<MultiTarget>,
    sql: String,
    mode: MultiMode,
    max_rows: Option<u64>,
    max_parallel: Option<usize>,
    on_event: Channel<MultiEvent>,
) -> Result<MultiRunHandle, IpcError> {
    let total: usize = targets.iter().map(|t| t.databases.len()).sum();

    let run_id = new_id();
    let cancel = CancelToken::new();
    state
        .running
        .lock()
        .expect("running-queries mutex poisoned")
        .insert(run_id.clone(), cancel.clone());

    if on_event
        .send(MultiEvent::Started {
            run_id: run_id.clone(),
            total,
        })
        .is_err()
    {
        state
            .running
            .lock()
            .expect("running-queries mutex poisoned")
            .remove(&run_id);
        return Ok(MultiRunHandle { run_id });
    }

    tracing::info!(%run_id, targets = targets.len(), total, mode = ?mode, "multi-target run started");

    let parallel = max_parallel.unwrap_or(4).max(1);
    let task_run_id = run_id.clone();
    tauri::async_runtime::spawn(async move {
        run_multi_task::<R>(
            app,
            task_run_id,
            targets,
            sql,
            mode,
            max_rows,
            on_event,
            cancel,
            parallel,
        )
        .await;
    });

    Ok(MultiRunHandle { run_id })
}

/// The detached body of a [`multi_target_run`]: fan out across targets, then
/// emit the terminal event and deregister the cancellation token.
#[allow(clippy::too_many_arguments)]
async fn run_multi_task<R: Runtime>(
    app: AppHandle<R>,
    run_id: String,
    targets: Vec<MultiTarget>,
    sql: String,
    mode: MultiMode,
    max_rows: Option<u64>,
    on_event: Channel<MultiEvent>,
    cancel: CancelToken,
    parallel: usize,
) {
    let sql = Arc::new(sql);
    let total: usize = targets.iter().map(|t| t.databases.len()).sum();
    let meta = Arc::new(MetaState {
        ref_cols: Mutex::new(None),
    });
    let succeeded = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let rows_total = Arc::new(AtomicU64::new(0));
    let sem = Arc::new(Semaphore::new(parallel));

    let mut set: JoinSet<()> = JoinSet::new();
    for target in targets {
        let app = app.clone();
        let channel = on_event.clone();
        let cancel = cancel.clone();
        let sql = sql.clone();
        let meta = meta.clone();
        let succeeded = succeeded.clone();
        let failed = failed.clone();
        let rows_total = rows_total.clone();
        let sem = sem.clone();
        set.spawn(async move {
            // Bound how many servers run concurrently.
            let _permit = match sem.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return,
            };
            if cancel.is_cancelled() {
                return;
            }
            run_target::<R>(
                app, channel, cancel, sql, mode, target, total, max_rows, meta, succeeded, failed,
                rows_total,
            )
            .await;
        });
    }
    while set.join_next().await.is_some() {}

    let state = app.state::<AppState>();
    state
        .running
        .lock()
        .expect("running-queries mutex poisoned")
        .remove(&run_id);

    let succeeded = succeeded.load(Ordering::SeqCst);
    let failed = failed.load(Ordering::SeqCst);
    let rows_total = rows_total.load(Ordering::SeqCst);

    let terminal = if cancel.is_cancelled() {
        tracing::info!(%run_id, succeeded, failed, "multi-target run cancelled");
        MultiEvent::Cancelled
    } else {
        tracing::info!(%run_id, succeeded, failed, rows_total, "multi-target run finished");
        MultiEvent::Finished {
            succeeded: succeeded as usize,
            failed: failed as usize,
            rows_total,
        }
    };
    let _ = on_event.send(terminal);
}

/// Run `sql` on every database of one target, sequentially. Opens a transient
/// connection, enforces the guard, and reports per-database progress.
#[allow(clippy::too_many_arguments)]
async fn run_target<R: Runtime>(
    app: AppHandle<R>,
    channel: Channel<MultiEvent>,
    cancel: CancelToken,
    sql: Arc<String>,
    mode: MultiMode,
    target: MultiTarget,
    total: usize,
    max_rows: Option<u64>,
    meta: Arc<MetaState>,
    succeeded: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    rows_total: Arc<AtomicU64>,
) {
    let state = app.state::<AppState>();
    let connection_id = target.connection_id;
    let db_count = target.databases.len() as u64;

    // A whole-server skip: count its databases as failed and report once.
    let server_error = |server: String, error: String| {
        failed.fetch_add(db_count, Ordering::SeqCst);
        let _ = channel.send(MultiEvent::ServerError {
            connection_id: connection_id.clone(),
            server,
            error,
        });
    };

    let spec = match state.store.get(&connection_id) {
        Ok(Some(spec)) => spec,
        Ok(None) => return server_error(connection_id.clone(), "no saved connection".to_string()),
        Err(err) => return server_error(connection_id.clone(), IpcError::from(err).message),
    };
    let server = spec.name.clone();

    // Defence-in-depth guard: refuse a blocked batch (e.g. non-SELECT on a
    // read-only connection). The UI also runs one upfront `guard_check`.
    if classify(&sql, spec.read_only).level == GuardLevel::Block {
        return server_error(
            server,
            "blocked by the SQL guard (read-only connection)".to_string(),
        );
    }

    let secret = match state.cached_secret(&connection_id) {
        Ok(Some(secret)) => secret,
        Ok(None) => {
            return server_error(
                server,
                "no stored password — connect to it once first".to_string(),
            )
        }
        Err(err) => return server_error(server, IpcError::from(err).message),
    };

    let connect = match driver_for(spec.driver) {
        Ok(driver) => driver.connect(&spec, &secret).await,
        Err(err) => Err(err),
    };
    let mut conn = match connect {
        Ok(conn) => conn,
        Err(err) => return server_error(server, IpcError::from(err).message),
    };

    let opts = ExecOptions {
        // Execute discards rows, so leave it uncapped to guarantee every
        // statement runs; results caps per database (mirrors `query_run`).
        max_rows: match mode {
            MultiMode::Execute => None,
            MultiMode::Results => max_rows.or(Some(DEFAULT_MAX_ROWS)),
        },
        batch_size: BATCH_SIZE,
    };

    for database in target.databases {
        if cancel.is_cancelled() {
            break;
        }
        let index = (succeeded.load(Ordering::SeqCst) + failed.load(Ordering::SeqCst)) as usize + 1;
        let _ = channel.send(MultiEvent::Target {
            connection_id: connection_id.clone(),
            server: server.clone(),
            database: database.clone(),
            index,
            total,
        });

        if let Err(err) = conn.use_database(&database).await {
            failed.fetch_add(1, Ordering::SeqCst);
            let _ = channel.send(MultiEvent::TargetDone {
                connection_id: connection_id.clone(),
                server: server.clone(),
                database,
                index,
                rows: None,
                error: Some(IpcError::from(err).message),
            });
            continue;
        }

        // `rows` carries the rows *returned* (results mode) or the rows
        // *affected* (execute mode, when a DML statement reported a count;
        // `None` for DDL/SELECT with no row-count).
        let result: Result<Option<u64>, _> = match mode {
            MultiMode::Results => {
                let mut sink = AggSink::new(
                    channel.clone(),
                    server.clone(),
                    database.clone(),
                    meta.clone(),
                    rows_total.clone(),
                );
                conn.execute(&sql, &opts, &mut sink, &cancel)
                    .await
                    .map(|_| Some(sink.rows_for_target))
            }
            MultiMode::Execute => {
                let mut sink = CountSink::default();
                conn.execute(&sql, &opts, &mut sink, &cancel)
                    .await
                    .map(|_| sink.had_affected.then_some(sink.affected))
            }
        };

        match result {
            Ok(rows) => {
                succeeded.fetch_add(1, Ordering::SeqCst);
                let done =
                    (succeeded.load(Ordering::SeqCst) + failed.load(Ordering::SeqCst)) as usize;
                let _ = channel.send(MultiEvent::TargetDone {
                    connection_id: connection_id.clone(),
                    server: server.clone(),
                    database,
                    index: done,
                    rows,
                    error: None,
                });
            }
            Err(selene_core::CoreError::Cancelled) => break,
            Err(err) => {
                failed.fetch_add(1, Ordering::SeqCst);
                let done =
                    (succeeded.load(Ordering::SeqCst) + failed.load(Ordering::SeqCst)) as usize;
                let _ = channel.send(MultiEvent::TargetDone {
                    connection_id: connection_id.clone(),
                    server: server.clone(),
                    database,
                    index: done,
                    rows: None,
                    error: Some(IpcError::from(err).message),
                });
            }
        }
    }
    // `conn` is dropped here, closing the transient connection.
}

/// Cancel an in-flight [`multi_target_run`]. Flips the shared cancellation
/// token; the task observes it between databases. Unknown/finished ids are a
/// no-op. (Identical mechanism to [`query_cancel`](super::query::query_cancel).)
#[tauri::command]
pub async fn multi_target_cancel(
    state: State<'_, AppState>,
    run_id: String,
) -> Result<(), IpcError> {
    let token = state
        .running
        .lock()
        .expect("running-queries mutex poisoned")
        .get(&run_id)
        .cloned();
    if let Some(token) = token {
        token.cancel();
        tracing::info!(%run_id, "multi-target run cancel requested");
    }
    Ok(())
}

// --- export an already-collected result set --------------------------------

/// Write an already-collected result set (`columns` + `rows`) to `path` in
/// `format`, via the core [`Exporter`]. Backs "Save CSV" for the aggregated
/// multi-target grid without re-running the queries. The CSV correctness (BOM,
/// quoting, line endings) is owned by `selene-core`, not duplicated here.
#[tauri::command]
pub async fn export_result_set(
    columns: Vec<Column>,
    rows: Vec<Vec<CellValue>>,
    format: ExportFormat,
    path: String,
    csv_options: Option<CsvExportOptions>,
) -> Result<ExportSummary, IpcError> {
    let target = PathBuf::from(&path);
    let csv_opts = csv_options.unwrap_or_default().into_core();

    let mut exporter = Exporter::create(format, &target, csv_opts)?;
    let row_count = rows.len();
    // `Exporter` is a `RowSink`; feed it the one set directly. Write errors are
    // stashed and re-raised by `finish`, so we must always call it.
    exporter.on_meta(0, columns).await;
    exporter.on_rows(0, rows).await;
    let summary = exporter.finish()?;

    tracing::info!(format = ?format, rows = row_count, "result set exported");
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ordinal: usize) -> Column {
        Column {
            name: name.to_string(),
            ordinal,
            db_type: "int".to_string(),
            logical: LogicalType::Integer,
            nullable: Some(true),
        }
    }

    #[test]
    fn unify_prepends_identity_columns_and_reordinals() {
        let unified = unify_columns(vec![col("a", 0), col("b", 1)]);
        let names: Vec<_> = unified.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["_server", "_database", "a", "b"]);
        // Ordinals are contiguous after the two prefix columns.
        assert_eq!(
            unified.iter().map(|c| c.ordinal).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        // The prefix columns are non-null text.
        assert_eq!(unified[0].logical, LogicalType::Text);
        assert_eq!(unified[0].nullable, Some(false));
    }

    #[test]
    fn prefixed_row_prepends_server_and_database() {
        let row = vec![CellValue::I64(1), CellValue::String("x".into())];
        let out = prefixed_row(row, 2, "srv", "db1");
        assert_eq!(
            out,
            vec![
                CellValue::String("srv".into()),
                CellValue::String("db1".into()),
                CellValue::I64(1),
                CellValue::String("x".into()),
            ]
        );
    }

    #[test]
    fn prefixed_row_pads_short_rows_with_null() {
        // A target returning fewer columns than the reference is padded.
        let out = prefixed_row(vec![CellValue::I64(1)], 3, "srv", "db");
        assert_eq!(out.len(), 5); // 2 prefix + 3 aligned
        assert_eq!(out[3], CellValue::Null);
        assert_eq!(out[4], CellValue::Null);
    }

    #[test]
    fn prefixed_row_truncates_long_rows() {
        // A target returning extra columns is truncated to the reference width.
        let out = prefixed_row(
            vec![CellValue::I64(1), CellValue::I64(2), CellValue::I64(3)],
            1,
            "srv",
            "db",
        );
        assert_eq!(out.len(), 3); // 2 prefix + 1 aligned
        assert_eq!(out[2], CellValue::I64(1));
    }

    #[test]
    fn count_sink_sums_affected_and_tracks_presence() {
        let mut sink = CountSink::default();
        tauri::async_runtime::block_on(async {
            sink.on_set_end(0, Some(3)).await;
            sink.on_set_end(1, Some(0)).await;
            // Discarded — must not affect the count.
            sink.on_rows(0, vec![vec![CellValue::I64(1)]]).await;
        });
        assert_eq!(sink.affected, 3);
        assert!(sink.had_affected);

        // No row-count reported (DDL / SELECT) → had_affected stays false.
        let mut ddl = CountSink::default();
        tauri::async_runtime::block_on(async {
            ddl.on_set_end(0, None).await;
        });
        assert_eq!(ddl.affected, 0);
        assert!(!ddl.had_affected);
    }

    #[test]
    fn collect_column_sink_grabs_first_cell_as_text() {
        let mut sink = CollectColumnSink::default();
        let rows = vec![
            vec![CellValue::String("e_alpha".into()), CellValue::I64(1)],
            vec![CellValue::String("e_beta".into()), CellValue::I64(2)],
        ];
        // Drive the async sink on a throwaway runtime; only set 0 is collected.
        tauri::async_runtime::block_on(async {
            sink.on_rows(0, rows).await;
            sink.on_rows(1, vec![vec![CellValue::String("ignored".into())]])
                .await;
        });
        assert_eq!(sink.values, ["e_alpha", "e_beta"]);
    }
}
