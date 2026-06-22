//! Session lifecycle commands: open and close a live database connection.
//!
//! A session is one live [`Connection`](selene_core::Connection) plus the
//! metadata other commands need (driver id, capabilities, originating
//! connection id, read-only flag). It is stored in
//! [`AppState::sessions`](crate::state::AppState::sessions) keyed by an opaque
//! `session_id`.

use serde::Serialize;
use tauri::State;

use selene_core::{driver_for, DriverCapabilities, DriverId, Secret};

use crate::error::IpcError;
use crate::state::{new_id, AppState, SessionEntry};

/// Returned by [`session_connect`]: the handle plus the driver facts the UI
/// needs to gate features (capabilities) without another round-trip.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Opaque id for the live session; pass it to every session-scoped command.
    pub session_id: String,
    /// Which backend the session is connected to.
    pub driver: DriverId,
    /// The driver's capability flags, snapshotted at connect time.
    pub capabilities: DriverCapabilities,
}

/// Open a live session for a saved connection.
///
/// Loads the spec from the store, obtains its password, connects via the driver,
/// and registers the live connection under a fresh `session_id`.
///
/// The password is resolved one of two ways:
/// - `password = None` (the normal path): read from the keychain via the
///   in-process cache. A missing secret is a hard error with `kind = "secret"`,
///   which the frontend uses to prompt the user for one and retry.
/// - `password = Some(_)`: connect with the supplied password (the retry after
///   such a prompt). Because Selene's model is keychain-first, a supplied
///   password that *successfully authenticates* is then persisted to the
///   keychain (and cache), so subsequent connects are silent. A wrong password
///   surfaces the driver's auth error and persists nothing.
#[tauri::command]
pub async fn session_connect(
    state: State<'_, AppState>,
    connection_id: String,
    password: Option<String>,
) -> Result<SessionInfo, IpcError> {
    let spec = state
        .store
        .get(&connection_id)?
        .ok_or_else(|| IpcError::unknown_connection(&connection_id))?;

    // Resolve the secret. A user-supplied password is persisted only after it
    // authenticates (see below); a missing stored secret is a hard error here
    // (unlike `connection_test`, which can probe with an empty password) so the
    // frontend can prompt and retry. Reading through the in-process cache means
    // only the first connect of a connection this session hits the keychain
    // (and its macOS access prompt).
    let (secret, persist) = match password {
        Some(password) => (Secret::new(password), true),
        None => {
            let secret = state.cached_secret(&connection_id)?.ok_or_else(|| {
                IpcError::new(
                    "secret",
                    format!("no stored password for connection '{connection_id}'"),
                )
            })?;
            (secret, false)
        }
    };

    let driver = driver_for(spec.driver)?;
    let caps = driver.capabilities();
    let conn = driver.connect(&spec, &secret).await?;

    // The password worked — remember it so future connects don't re-prompt.
    if persist {
        state.keychain.set_secret(&connection_id, &secret)?;
        state.cache_secret(&connection_id, secret);
    }

    let session_id = new_id();
    let entry = SessionEntry {
        driver: spec.driver,
        caps,
        connection_id: connection_id.clone(),
        read_only: spec.read_only,
        conn,
    };
    state
        .sessions
        .lock()
        .await
        .insert(session_id.clone(), entry);

    tracing::info!(
        %session_id,
        %connection_id,
        driver = ?spec.driver,
        read_only = spec.read_only,
        "session connected"
    );
    Ok(SessionInfo {
        session_id,
        driver: spec.driver,
        capabilities: caps,
    })
}

/// Switch the active database for a session.
///
/// Runs `USE [<database>]` on the connection. The bracket-quoting is handled by
/// the driver so the name is safe to pass as user input (no SQL injection).
#[tauri::command]
pub async fn session_use_database(
    state: State<'_, AppState>,
    session_id: String,
    database: String,
) -> Result<(), IpcError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| IpcError::unknown_session(&session_id))?;
    session.conn.use_database(&database).await?;
    tracing::info!(%session_id, %database, "session database switched");
    Ok(())
}

/// Create a new database on the session's server.
///
/// Refused on a read-only connection: this is DDL and does not pass through the
/// SQL guard. The driver bracket-quotes the name (safe as input).
#[tauri::command]
pub async fn session_create_database(
    state: State<'_, AppState>,
    session_id: String,
    database: String,
) -> Result<(), IpcError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| IpcError::unknown_session(&session_id))?;
    if session.read_only {
        return Err(IpcError::new(
            "blocked",
            "connection is read-only; cannot create a database",
        ));
    }
    session.conn.create_database(&database).await?;
    tracing::info!(%session_id, %database, "database created");
    Ok(())
}

/// Drop a database on the session's server.
///
/// Refused on a read-only connection: this is destructive DDL and does not pass
/// through the SQL guard. Fails if the database is in use by other connections.
#[tauri::command]
pub async fn session_drop_database(
    state: State<'_, AppState>,
    session_id: String,
    database: String,
) -> Result<(), IpcError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| IpcError::unknown_session(&session_id))?;
    if session.read_only {
        return Err(IpcError::new(
            "blocked",
            "connection is read-only; cannot drop a database",
        ));
    }
    session.conn.drop_database(&database).await?;
    tracing::info!(%session_id, %database, "database dropped");
    Ok(())
}

/// Rename a database on the session's server.
///
/// Refused on a read-only connection: this is destructive DDL and does not pass
/// through the SQL guard. The driver bracket-quotes both names (safe as input).
///
/// With `force == false` the rename fails fast (kind `"database_in_use"`) when
/// the database is in use, so the frontend can prompt before retrying with
/// `force == true`, which disconnects the active sessions to complete it.
#[tauri::command]
pub async fn session_rename_database(
    state: State<'_, AppState>,
    session_id: String,
    from: String,
    to: String,
    force: bool,
) -> Result<(), IpcError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| IpcError::unknown_session(&session_id))?;
    if session.read_only {
        return Err(IpcError::new(
            "blocked",
            "connection is read-only; cannot rename a database",
        ));
    }
    session.conn.rename_database(&from, &to, force).await?;
    tracing::info!(%session_id, %from, %to, force, "database renamed");
    Ok(())
}

/// Bring a database online or take it offline.
///
/// Taking a database offline terminates all other connections to it
/// (`ROLLBACK IMMEDIATE`). Refused on a read-only connection.
#[tauri::command]
pub async fn session_set_database_online(
    state: State<'_, AppState>,
    session_id: String,
    database: String,
    online: bool,
) -> Result<(), IpcError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| IpcError::unknown_session(&session_id))?;
    if session.read_only {
        return Err(IpcError::new(
            "blocked",
            "connection is read-only; cannot change database state",
        ));
    }
    session.conn.set_database_online(&database, online).await?;
    tracing::info!(%session_id, %database, online, "database state changed");
    Ok(())
}

/// Return the name of the current database for the session.
///
/// Useful after a `USE <database>` statement — the frontend calls this to
/// reflect the new context in the toolbar without a full reconnect.
#[tauri::command]
pub async fn session_current_database(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<String, IpcError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| IpcError::unknown_session(&session_id))?;
    let db = session.conn.current_database().await?;
    Ok(db)
}

/// Close a live session, dropping its connection.
///
/// Removing an unknown or already-closed session id is a no-op (the desired
/// end-state — no live session — is reached either way).
#[tauri::command]
pub async fn session_disconnect(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), IpcError> {
    let removed = state.sessions.lock().await.remove(&session_id).is_some();
    // Dropping the `SessionEntry` (and its `Box<dyn Connection>`) closes the
    // underlying connection.
    tracing::info!(%session_id, was_open = removed, "session disconnected");
    Ok(())
}
