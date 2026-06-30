//! Streaming database backup & restore commands.
//!
//! `BACKUP`/`RESTORE` are server-side, single-statement operations that can run
//! for a while and report no rows. Two design choices follow from that:
//!
//! 1. **Dedicated connection.** The operation runs on a *fresh* connection
//!    opened from the same saved spec — not the caller's interactive session.
//!    So a long backup never blocks the session's mutex, and cancelling (which
//!    must `KILL` the running request from another connection) drops only this
//!    throwaway connection, leaving the user's session intact.
//! 2. **Out-of-band progress.** tiberius drops the server's `STATS` info
//!    messages, so a *second* connection polls
//!    [`backup_percent_complete`](selene_core::Connection::backup_percent_complete)
//!    against the operation connection's `@@SPID` and forwards
//!    `Progress { percent }` events. The poller also performs the `KILL` on
//!    cancel. If it cannot be opened (or the login lacks `VIEW SERVER STATE`),
//!    progress is simply absent and the UI shows indeterminate progress.
//!
//! Both commands **await** to completion (like `export_result`) and emit a
//! terminal `Done` / `Cancelled` / `Failed` event on the channel.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::State;

use selene_core::{
    driver_for, plan_moves, BackupFile, BackupOptions, CancelToken, Connection, ConnectionSpec,
    CoreError, DefaultDirs, RestoreOptions, Secret,
};

use crate::commands::{BackupEvent, RestoreEvent};
use crate::error::IpcError;
use crate::state::{new_id, AppState};

/// How often the side connection samples `percent_complete`. Also the maximum
/// latency between a cancel request and the `KILL` that enacts it.
const POLL_INTERVAL: Duration = Duration::from_millis(700);

/// Backup options from the frontend (camelCase on the wire).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupOptionsArg {
    pub compression: bool,
    pub checksum: bool,
    pub verify_after: bool,
}

impl From<BackupOptionsArg> for BackupOptions {
    fn from(a: BackupOptionsArg) -> Self {
        BackupOptions {
            compression: a.compression,
            checksum: a.checksum,
            verify_after: a.verify_after,
        }
    }
}

/// Restore options from the frontend (camelCase on the wire).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOptionsArg {
    pub checksum: bool,
}

impl From<RestoreOptionsArg> for RestoreOptions {
    fn from(a: RestoreOptionsArg) -> Self {
        RestoreOptions {
            checksum: a.checksum,
        }
    }
}

/// Returned by the awaited commands. The channel events carry the real status;
/// this is a small summary for the resolved promise.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationSummary {
    /// Wall-clock duration of the operation, in milliseconds.
    pub elapsed_ms: u64,
    /// Whether the operation was cancelled by the user (vs. completed).
    pub cancelled: bool,
}

/// Resolve a session's originating connection id and read-only flag with a brief
/// lock (we do not hold the session for the operation — it runs on its own
/// connection).
async fn session_connection(
    state: &AppState,
    session_id: &str,
) -> Result<(String, bool), IpcError> {
    let sessions = state.sessions.lock().await;
    let session = sessions
        .get(session_id)
        .ok_or_else(|| IpcError::unknown_session(session_id))?;
    Ok((session.connection_id.clone(), session.read_only))
}

/// Open a fresh connection for `connection_id` from its stored spec + cached
/// secret. Returns the connection plus the spec/secret (cloned) so the caller
/// can open a second (poller) connection without re-reading state.
async fn open_operation_connection(
    state: &AppState,
    connection_id: &str,
) -> Result<(Box<dyn Connection>, ConnectionSpec, Secret), IpcError> {
    let spec = state
        .store
        .get(connection_id)?
        .ok_or_else(|| IpcError::unknown_connection(connection_id))?;
    let secret = state.cached_secret(connection_id)?.ok_or_else(|| {
        IpcError::new(
            "secret",
            "no cached credentials for this connection; reconnect and try again",
        )
    })?;
    let driver = driver_for(spec.driver)?;
    let conn = driver.connect(&spec, &secret).await?;
    Ok((conn, spec, secret))
}

/// Spawn the progress/cancel poller on its own connection.
///
/// It polls `percent_complete` for `spid` and forwards each new whole-percent
/// value via `on_percent` (which returns `false` if the listener is gone). When
/// `cancel` fires it issues `KILL <spid>` on its connection — the only way to
/// stop the single-statement backup/restore — and exits. If progress polling
/// errors (e.g. missing `VIEW SERVER STATE`) it stops sampling but keeps
/// watching for cancel so the `KILL` path still works.
fn spawn_poller<F>(
    spec: ConnectionSpec,
    secret: Secret,
    spid: i32,
    cancel: CancelToken,
    mut on_percent: F,
) -> tauri::async_runtime::JoinHandle<()>
where
    F: FnMut(f32) -> bool + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        let driver = match driver_for(spec.driver) {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut conn = match driver.connect(&spec, &secret).await {
            Ok(c) => c,
            Err(_) => return,
        };

        let mut last_pct = -1i32;
        let mut poll_ok = true;
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if cancel.is_cancelled() {
                let _ = conn.kill_session(spid).await;
                return;
            }
            if poll_ok {
                match conn.backup_percent_complete(spid).await {
                    Ok(Some(pct)) => {
                        let whole = pct.round() as i32;
                        if whole != last_pct {
                            last_pct = whole;
                            if !on_percent(pct) {
                                return; // listener gone
                            }
                        }
                    }
                    Ok(None) => {} // no active request yet / already finished
                    // Lost the ability to read progress (perms / transient).
                    // Keep looping so a later cancel can still KILL.
                    Err(_) => poll_ok = false,
                }
            }
        }
    })
}

/// Back up `database` to the server-side file `path`.
///
/// Allowed on a read-only connection: a backup only reads the database. The
/// operation runs on a dedicated connection; a second connection streams
/// progress and can `KILL` it on cancel. Returns once the backup ends.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn database_backup(
    state: State<'_, AppState>,
    session_id: String,
    database: String,
    path: String,
    options: BackupOptionsArg,
    on_progress: Channel<BackupEvent>,
) -> Result<OperationSummary, IpcError> {
    let fail = |ch: &Channel<BackupEvent>, e: IpcError| -> IpcError {
        let _ = ch.send(BackupEvent::Failed {
            message: e.message.clone(),
        });
        e
    };

    let (connection_id, _read_only) = match session_connection(&state, &session_id).await {
        Ok(v) => v,
        Err(e) => return Err(fail(&on_progress, e)),
    };

    let (mut op_conn, spec, secret) = match open_operation_connection(&state, &connection_id).await
    {
        Ok(v) => v,
        Err(e) => return Err(fail(&on_progress, e)),
    };

    // Register the cancel token and announce. The id is what `backup_cancel`
    // flips; the poller (holding a clone of the token) enacts the KILL.
    let operation_id = new_id();
    let cancel = CancelToken::new();
    state
        .running
        .lock()
        .expect("running mutex poisoned")
        .insert(operation_id.clone(), cancel.clone());

    if on_progress
        .send(BackupEvent::Started {
            operation_id: operation_id.clone(),
        })
        .is_err()
    {
        state
            .running
            .lock()
            .expect("running mutex poisoned")
            .remove(&operation_id);
        return Ok(OperationSummary {
            elapsed_ms: 0,
            cancelled: false,
        });
    }

    tracing::info!(%session_id, %operation_id, "backup started");

    // Best-effort progress + cancel poller, keyed on the operation connection's
    // server session id.
    let spid = op_conn.current_session_id().await.ok();
    let poller = spid.map(|spid| {
        let ch = on_progress.clone();
        spawn_poller(
            spec.clone(),
            secret.clone(),
            spid,
            cancel.clone(),
            move |p| ch.send(BackupEvent::Progress { percent: p }).is_ok(),
        )
    });

    let started = Instant::now();
    let opts: BackupOptions = options.into();
    let result = op_conn
        .backup_database(&database, &path, &opts, &cancel)
        .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    finish_operation(&state, &operation_id, poller).await;
    let cancelled = cancel.is_cancelled();

    match result {
        Ok(()) => {
            let _ = on_progress.send(BackupEvent::Done);
            tracing::info!(%session_id, %operation_id, elapsed_ms, "backup finished");
            Ok(OperationSummary {
                elapsed_ms,
                cancelled: false,
            })
        }
        Err(_) if cancelled => {
            let _ = on_progress.send(BackupEvent::Cancelled);
            tracing::info!(%session_id, %operation_id, "backup cancelled");
            Ok(OperationSummary {
                elapsed_ms,
                cancelled: true,
            })
        }
        Err(e) => Err(fail(&on_progress, e.into())),
    }
}

/// Restore the backup at `path` over the existing database `target`.
///
/// Refused on a read-only connection (it overwrites data). Runs `RESTORE
/// FILELISTONLY` + reads the target's current files to plan `MOVE` relocations,
/// then restores with `WITH REPLACE`. Same dedicated-connection + poller model
/// as [`database_backup`].
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn database_restore(
    state: State<'_, AppState>,
    session_id: String,
    target: String,
    path: String,
    options: RestoreOptionsArg,
    on_progress: Channel<RestoreEvent>,
) -> Result<OperationSummary, IpcError> {
    let fail = |ch: &Channel<RestoreEvent>, e: IpcError| -> IpcError {
        let _ = ch.send(RestoreEvent::Failed {
            message: e.message.clone(),
        });
        e
    };

    let (connection_id, read_only) = match session_connection(&state, &session_id).await {
        Ok(v) => v,
        Err(e) => return Err(fail(&on_progress, e)),
    };
    if read_only {
        return Err(fail(
            &on_progress,
            IpcError::new(
                "blocked",
                "connection is read-only; cannot restore over a database",
            ),
        ));
    }

    let (mut op_conn, spec, secret) = match open_operation_connection(&state, &connection_id).await
    {
        Ok(v) => v,
        Err(e) => return Err(fail(&on_progress, e)),
    };

    let operation_id = new_id();
    let cancel = CancelToken::new();
    state
        .running
        .lock()
        .expect("running mutex poisoned")
        .insert(operation_id.clone(), cancel.clone());

    if on_progress
        .send(RestoreEvent::Started {
            operation_id: operation_id.clone(),
        })
        .is_err()
    {
        state
            .running
            .lock()
            .expect("running mutex poisoned")
            .remove(&operation_id);
        return Ok(OperationSummary {
            elapsed_ms: 0,
            cancelled: false,
        });
    }

    tracing::info!(%session_id, %operation_id, "restore started");

    let spid = op_conn.current_session_id().await.ok();
    let poller = spid.map(|spid| {
        let ch = on_progress.clone();
        spawn_poller(
            spec.clone(),
            secret.clone(),
            spid,
            cancel.clone(),
            move |p| ch.send(RestoreEvent::Progress { percent: p }).is_ok(),
        )
    });

    let started = Instant::now();
    let opts: RestoreOptions = options.into();
    let result = restore_steps(op_conn.as_mut(), &target, &path, &opts, &cancel).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    finish_operation(&state, &operation_id, poller).await;
    let cancelled = cancel.is_cancelled();

    match result {
        Ok(()) => {
            let _ = on_progress.send(RestoreEvent::Done);
            tracing::info!(%session_id, %operation_id, elapsed_ms, "restore finished");
            Ok(OperationSummary {
                elapsed_ms,
                cancelled: false,
            })
        }
        Err(_) if cancelled => {
            let _ = on_progress.send(RestoreEvent::Cancelled);
            tracing::info!(%session_id, %operation_id, "restore cancelled");
            Ok(OperationSummary {
                elapsed_ms,
                cancelled: true,
            })
        }
        Err(e) => Err(fail(&on_progress, e.into())),
    }
}

/// The full restore sequence on one connection: discover the backup's files and
/// the target's current files, plan the `MOVE` relocations, then restore.
async fn restore_steps(
    conn: &mut dyn Connection,
    target: &str,
    path: &str,
    opts: &RestoreOptions,
    cancel: &CancelToken,
) -> Result<(), CoreError> {
    let backup_files = conn.restore_filelist(path).await?;
    let target_files = conn.database_files(target).await?;
    // Default dirs are only a fallback (when a target file's directory cannot be
    // derived); a failure to read them must not abort the restore.
    let default_dirs = conn.default_file_dirs().await.unwrap_or(DefaultDirs {
        data: String::new(),
        log: String::new(),
    });
    let moves = plan_moves(&backup_files, &target_files, &default_dirs, target);
    conn.restore_database(target, path, &moves, opts, cancel)
        .await
}

/// Common teardown: stop the poller (so no late `Progress` races the terminal
/// event) and deregister the cancel token.
async fn finish_operation(
    state: &AppState,
    operation_id: &str,
    poller: Option<tauri::async_runtime::JoinHandle<()>>,
) {
    if let Some(poller) = poller {
        poller.abort();
        let _ = poller.await;
    }
    state
        .running
        .lock()
        .expect("running mutex poisoned")
        .remove(operation_id);
}

/// Preview the logical files inside the backup at `path` (`RESTORE
/// FILELISTONLY`), run on the session connection. Lets the restore dialog show
/// what a `.bak` contains before committing.
#[tauri::command]
pub async fn restore_filelist(
    state: State<'_, AppState>,
    session_id: String,
    path: String,
) -> Result<Vec<BackupFile>, IpcError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| IpcError::unknown_session(&session_id))?;
    let files = session.conn.restore_filelist(&path).await?;
    Ok(files)
}

/// Request cancellation of an in-flight backup or restore.
///
/// Flips the operation's cancel token; the poller observes it and `KILL`s the
/// operation's connection (best-effort — it needs the poller connection and
/// `ALTER ANY CONNECTION`/sysadmin). Cancelling an unknown/finished id is a
/// no-op.
#[tauri::command]
pub async fn backup_cancel(
    state: State<'_, AppState>,
    operation_id: String,
) -> Result<(), IpcError> {
    let token = state
        .running
        .lock()
        .expect("running mutex poisoned")
        .get(&operation_id)
        .cloned();
    if let Some(token) = token {
        token.cancel();
        tracing::info!(%operation_id, "backup/restore cancel requested");
    }
    Ok(())
}
