//! Commands for managing saved connections and their secrets.
//!
//! Specs are persisted to `connections.json` via the
//! [`ConnectionStore`](crate::state::ConnectionStore); passwords live only in
//! the OS keychain. A command never logs or echoes a password — it is wrapped
//! in [`Secret`](selene_core::Secret) the moment it crosses this boundary.

use tauri::State;

use selene_core::{driver_for, ConnectionSpec, Secret, TestReport};

use crate::error::IpcError;
use crate::state::AppState;

/// List all saved connection specs (non-secret).
#[tauri::command]
pub async fn connections_list(state: State<'_, AppState>) -> Result<Vec<ConnectionSpec>, IpcError> {
    let specs = state.store.list()?;
    tracing::info!(count = specs.len(), "listed connections");
    Ok(specs)
}

/// Create or update a saved connection.
///
/// The spec is upserted into the store. If `password` is `Some`, it is written
/// to the keychain under `spec.id`, replacing any existing secret; if `None`,
/// any previously stored secret is left untouched (so editing a connection's
/// non-secret fields does not require re-entering the password).
#[tauri::command]
pub async fn connection_save(
    state: State<'_, AppState>,
    spec: ConnectionSpec,
    password: Option<String>,
) -> Result<ConnectionSpec, IpcError> {
    let id = spec.id.clone();
    let saved = state.store.upsert(spec)?;
    if let Some(password) = password {
        // Wrap immediately; never log or format the raw value.
        let secret = Secret::new(password);
        state.keychain.set_secret(&id, &secret)?;
        // Keep the in-process cache in step with the keychain so the next
        // connect uses the new password without a fresh keychain prompt.
        state.cache_secret(&id, secret);
        tracing::info!(connection_id = %id, "saved connection (secret updated)");
    } else {
        tracing::info!(connection_id = %id, "saved connection (secret unchanged)");
    }
    Ok(saved)
}

/// Delete a saved connection and its stored secret.
///
/// Both the store entry and the keychain entry are removed; each removal is
/// idempotent, so deleting an already-absent connection succeeds.
#[tauri::command]
pub async fn connection_delete(state: State<'_, AppState>, id: String) -> Result<(), IpcError> {
    state.store.delete(&id)?;
    state.keychain.delete_secret(&id)?;
    state.invalidate_cached_secret(&id);
    tracing::info!(connection_id = %id, "deleted connection");
    Ok(())
}

/// Reorder saved connections.
///
/// `ids` is the full list of connection ids in the desired display order. Ids
/// absent from the store are silently ignored; stored entries missing from
/// `ids` are kept at the end so nothing is ever silently dropped.
#[tauri::command]
pub async fn connection_reorder(
    state: State<'_, AppState>,
    ids: Vec<String>,
) -> Result<(), IpcError> {
    state.store.reorder(&ids)?;
    tracing::info!(count = ids.len(), "reordered connections");
    Ok(())
}

/// Import a list of connection specs (upsert by id).
///
/// Each spec is merged into the store: an existing spec with the same id is
/// updated in-place; a spec with an id not yet in the store is appended. Order
/// within the file is preserved for new entries. Passwords are never part of
/// a backup file — the user will be prompted when they first connect each
/// imported connection.
#[tauri::command]
pub async fn connections_import(
    state: State<'_, AppState>,
    specs: Vec<ConnectionSpec>,
) -> Result<Vec<ConnectionSpec>, IpcError> {
    let count = specs.len();
    for spec in specs {
        state.store.upsert(spec)?;
    }
    let all = state.store.list()?;
    tracing::info!(imported = count, total = all.len(), "imported connections");
    Ok(all)
}

/// Test connectivity for a spec without opening a persistent session.
///
/// The secret is taken from `password` when provided (for testing an
/// as-yet-unsaved form), otherwise read from the keychain under `spec.id`. A
/// missing keychain secret yields an empty `Secret`, letting the driver report
/// the authentication failure itself.
#[tauri::command]
pub async fn connection_test(
    state: State<'_, AppState>,
    spec: ConnectionSpec,
    password: Option<String>,
) -> Result<TestReport, IpcError> {
    let secret = match password {
        Some(password) => Secret::new(password),
        None => state
            .cached_secret(&spec.id)?
            .unwrap_or_else(|| Secret::new(String::new())),
    };
    let driver = driver_for(spec.driver)?;
    let report = driver.test_connection(&spec, &secret).await?;
    tracing::info!(
        connection_id = %spec.id,
        driver = ?spec.driver,
        elapsed_ms = report.elapsed_ms,
        "connection test ok"
    );
    Ok(report)
}
