//! The streaming query path — the core data flow of the app.
//!
//! [`query_run`] enforces the SQL guard, then spawns a detached task that
//! streams result-set events to the frontend over a `tauri::ipc::Channel`. The
//! command returns a [`QueryHandle`] immediately (it does **not** await the
//! task), so the UI gets the cancellation handle before the first row arrives.
//!
//! ## Cancellation (v0.1 limitation)
//! Cancellation is **cooperative**: the core checks the [`CancelToken`] between
//! row batches, so [`query_cancel`] interrupts a query *between batches*, not
//! mid-batch. A query that is blocked server-side **before its first batch**
//! (e.g. waiting on a lock, or a long-running aggregate that has not yet
//! produced rows) will not stop instantly — the token is only observed once the
//! driver next yields control. A hard server-side cancel (dropping the
//! connection to raise an Attention, plus connection pooling so the session
//! stays usable) is deferred past v0.1.

use std::time::Instant;

use async_trait::async_trait;
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Runtime, State};

use selene_core::{
    classify, classify_for, CancelToken, CellValue, Column, ExecOptions, Flow, GuardLevel,
    GuardVerdict, RowSink,
};

use crate::commands::QueryEvent;
use crate::error::IpcError;
use crate::state::{new_id, AppState};

/// Default cap on rows streamed when the caller does not specify `maxRows`.
/// Mirrors [`ExecOptions::default`]'s cap so behaviour is consistent whether or
/// not the frontend passes a limit.
const DEFAULT_MAX_ROWS: u64 = 50_000;

/// Rows buffered per batch before flushing to the channel. A batch is also the
/// granularity at which cooperative cancellation is observed.
const BATCH_SIZE: usize = 500;

/// Returned by [`query_run`]: the id used to [`query_cancel`] this run. Result
/// data arrives asynchronously on the `Channel`, not in this return value.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryHandle {
    /// Opaque id for the in-flight query; pass to `query_cancel`.
    pub query_id: String,
}

/// A [`RowSink`] that forwards driver events to the frontend as [`QueryEvent`]s
/// over a `tauri::ipc::Channel`.
///
/// If a `channel.send` fails — which happens when the webview/listener has gone
/// away (window closed, listener dropped) — there is no point producing more
/// rows, so the sink returns [`Flow::Stop`] to wind the query down promptly.
///
/// The `Channel` is a concrete type, so the sink needs no runtime generic and
/// stays trivially `Send` (which `RowSink` requires).
struct ChannelSink {
    channel: Channel<QueryEvent>,
}

impl ChannelSink {
    fn new(channel: Channel<QueryEvent>) -> Self {
        Self { channel }
    }

    /// Send one event; map a transport failure (listener gone) to `Flow::Stop`.
    fn forward(&self, event: QueryEvent) -> Flow {
        match self.channel.send(event) {
            Ok(()) => Flow::Continue,
            Err(_) => Flow::Stop,
        }
    }
}

#[async_trait]
impl RowSink for ChannelSink {
    async fn on_meta(&mut self, set_index: usize, columns: Vec<Column>) -> Flow {
        self.forward(QueryEvent::Meta { set_index, columns })
    }

    async fn on_rows(&mut self, set_index: usize, rows: Vec<Vec<CellValue>>) -> Flow {
        self.forward(QueryEvent::Rows { set_index, rows })
    }

    async fn on_set_end(&mut self, set_index: usize, affected: Option<u64>) -> Flow {
        self.forward(QueryEvent::SetEnd {
            set_index,
            affected,
        })
    }
}

/// Classify a SQL batch for safety. A thin wrapper over
/// [`selene_core::classify`]; `read_only` reflects the connection's safety
/// toggle. Pure and synchronous, but exposed as a command so the editor can
/// show a warning before the user runs anything.
///
/// This advisory pre-check stays SQL-only for now: making it driver-aware (so a
/// MongoDB tab pre-flights through [`classify_mongo`](selene_core::classify_mongo))
/// requires the frontend to pass the session, which lands in M4 when MongoDB is
/// wired into the UI. The **authoritative** guard is enforced server-side in
/// [`query_run`], which *is* driver-aware — so a write on a read-only MongoDB
/// connection is refused there regardless of this pre-check.
#[tauri::command]
pub async fn guard_check(sql: String, read_only: bool) -> Result<GuardVerdict, IpcError> {
    Ok(classify(&sql, read_only))
}

/// Run a SQL batch, streaming results to the frontend over `on_event`.
///
/// Flow:
/// 1. Look up the session; enforce the SQL guard using the session's cached
///    `read_only` flag. A [`GuardLevel::Block`] verdict returns an error and
///    the query never runs.
/// 2. Mint a `query_id` + [`CancelToken`], register the token, and emit
///    `Started`.
/// 3. Spawn a detached task that locks the session, runs
///    [`Connection::execute`](selene_core::Connection::execute) into a
///    [`ChannelSink`], and emits `Finished` / `Cancelled` / `Failed`, then
///    deregisters the token.
/// 4. Return the [`QueryHandle`] immediately.
#[tauri::command]
pub async fn query_run<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    session_id: String,
    sql: String,
    max_rows: Option<u64>,
    on_event: Channel<QueryEvent>,
) -> Result<QueryHandle, IpcError> {
    // (1) Resolve the session's driver + read-only flag (cheaply, without holding
    //     the lock for the whole query) and enforce the guard up front. The
    //     driver selects the classifier: MongoDB queries are mongosh method calls,
    //     not SQL, so `classify_for` routes them through the Mongo guard.
    let (driver, read_only) = {
        let sessions = state.sessions.lock().await;
        let session = sessions
            .get(&session_id)
            .ok_or_else(|| IpcError::unknown_session(&session_id))?;
        (session.driver, session.read_only)
    };

    let verdict = classify_for(driver, &sql, read_only);
    if verdict.level == GuardLevel::Block {
        tracing::warn!(%session_id, "query blocked by SQL guard");
        return Err(IpcError::blocked(&verdict.reasons));
    }

    // (2) Register a cancellation token and announce the query.
    let query_id = new_id();
    let cancel = CancelToken::new();
    state
        .running
        .lock()
        .expect("running-queries mutex poisoned")
        .insert(query_id.clone(), cancel.clone());

    if on_event
        .send(QueryEvent::Started {
            query_id: query_id.clone(),
        })
        .is_err()
    {
        // The listener is already gone; nothing to run. Clean up the token.
        state
            .running
            .lock()
            .expect("running-queries mutex poisoned")
            .remove(&query_id);
        tracing::info!(%query_id, "query listener gone before start; aborting");
        return Ok(QueryHandle { query_id });
    }

    tracing::info!(%session_id, %query_id, "query started");

    // (3) Spawn the streaming task. It owns a clone of the `AppHandle` and
    //     re-acquires `AppState` inside, so nothing borrows the command's
    //     short-lived `State`.
    let task_query_id = query_id.clone();
    tauri::async_runtime::spawn(async move {
        run_query_task::<R>(
            app,
            session_id,
            sql,
            max_rows,
            on_event,
            cancel,
            task_query_id,
        )
        .await;
    });

    // (4) Hand the cancellation id back without awaiting the task.
    Ok(QueryHandle { query_id })
}

/// The detached body of a `query_run`: execute, stream, and emit a terminal
/// event. Always deregisters the cancellation token before returning.
#[allow(clippy::too_many_arguments)]
async fn run_query_task<R: Runtime>(
    app: AppHandle<R>,
    session_id: String,
    sql: String,
    max_rows: Option<u64>,
    on_event: Channel<QueryEvent>,
    cancel: CancelToken,
    query_id: String,
) {
    use tauri::Manager;
    let state = app.state::<AppState>();

    let opts = ExecOptions {
        max_rows: max_rows.or(Some(DEFAULT_MAX_ROWS)),
        batch_size: BATCH_SIZE,
    };

    let started = Instant::now();
    let mut sink = ChannelSink::new(on_event.clone());

    // Lock the session for the duration of the query. Concurrent queries on the
    // *same* session serialize here (documented v0.1 behaviour); different
    // sessions run in parallel.
    let result = {
        let mut sessions = state.sessions.lock().await;
        match sessions.get_mut(&session_id) {
            Some(session) => session.conn.execute(&sql, &opts, &mut sink, &cancel).await,
            None => Err(selene_core::CoreError::Config(format!(
                "session '{session_id}' disconnected before the query ran"
            ))),
        }
    };

    let elapsed_ms = started.elapsed().as_millis() as u64;

    // Emit the terminal event. A cancelled run is reported as `Cancelled` even
    // though the core surfaces it as `CoreError::Cancelled`, so the UI can
    // distinguish a user cancel from a real failure.
    let terminal = match result {
        Ok(outcome) => {
            tracing::info!(
                %query_id,
                result_sets = outcome.result_sets,
                total_rows = outcome.total_rows,
                truncated = outcome.truncated,
                elapsed_ms,
                "query finished"
            );
            QueryEvent::Finished {
                outcome,
                elapsed_ms,
            }
        }
        Err(selene_core::CoreError::Cancelled) => {
            tracing::info!(%query_id, elapsed_ms, "query cancelled");
            QueryEvent::Cancelled
        }
        Err(err) => {
            let ipc: IpcError = err.into();
            tracing::warn!(%query_id, kind = %ipc.kind, "query failed");
            QueryEvent::Failed {
                message: ipc.message,
            }
        }
    };
    // Ignore a send error here: if the listener is gone there is nothing to do.
    let _ = on_event.send(terminal);

    // Always deregister the token so `running` does not leak entries.
    state
        .running
        .lock()
        .expect("running-queries mutex poisoned")
        .remove(&query_id);
}

/// Request cancellation of an in-flight query.
///
/// Looks up the query's [`CancelToken`] and flips it; the running task observes
/// it between row batches and ends with a `Cancelled` event. Cancelling an
/// unknown or already-finished `query_id` is a no-op.
#[tauri::command]
pub async fn query_cancel(state: State<'_, AppState>, query_id: String) -> Result<(), IpcError> {
    let token = state
        .running
        .lock()
        .expect("running-queries mutex poisoned")
        .get(&query_id)
        .cloned();
    match token {
        Some(token) => {
            token.cancel();
            tracing::info!(%query_id, "query cancel requested");
        }
        None => {
            tracing::info!(%query_id, "query cancel for unknown/finished query (no-op)");
        }
    }
    Ok(())
}
