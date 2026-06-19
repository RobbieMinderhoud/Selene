//! Schema-introspection commands: walk the object tree one level at a time.
//!
//! Each command locks the target session and forwards to the corresponding
//! [`Connection`](selene_core::Connection) method. Introspection is lazy per
//! level (databases → schemas → tables → columns) so a server with thousands of
//! objects is never loaded all at once.

use tauri::State;

use selene_core::{ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo};

use crate::error::IpcError;
use crate::state::AppState;

/// List databases on the session's server.
#[tauri::command]
pub async fn databases_list(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<DatabaseInfo>, IpcError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| IpcError::unknown_session(&session_id))?;
    let out = session.conn.list_databases().await?;
    tracing::info!(%session_id, count = out.len(), "listed databases");
    Ok(out)
}

/// List schemas within `database`.
#[tauri::command]
pub async fn schemas_list(
    state: State<'_, AppState>,
    session_id: String,
    database: String,
) -> Result<Vec<SchemaInfo>, IpcError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| IpcError::unknown_session(&session_id))?;
    let out = session.conn.list_schemas(&database).await?;
    tracing::info!(%session_id, count = out.len(), "listed schemas");
    Ok(out)
}

/// List tables and views within `database`.`schema`.
#[tauri::command]
pub async fn tables_list(
    state: State<'_, AppState>,
    session_id: String,
    database: String,
    schema: String,
) -> Result<Vec<TableInfo>, IpcError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| IpcError::unknown_session(&session_id))?;
    let out = session.conn.list_tables(&database, &schema).await?;
    tracing::info!(%session_id, count = out.len(), "listed tables");
    Ok(out)
}

/// List columns of `database`.`schema`.`table`.
#[tauri::command]
pub async fn columns_list(
    state: State<'_, AppState>,
    session_id: String,
    database: String,
    schema: String,
    table: String,
) -> Result<Vec<ColumnInfo>, IpcError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| IpcError::unknown_session(&session_id))?;
    let out = session
        .conn
        .list_columns(&database, &schema, &table)
        .await?;
    tracing::info!(%session_id, count = out.len(), "listed columns");
    Ok(out)
}
