//! Connection health monitoring: a background heartbeat that detects dead
//! sessions and auto-closes them.
//!
//! ## Why this exists
//! A live session wraps a single TDS connection. When the network drops
//! *silently* (Wi-Fi off, VPN down, laptop sleeps) the socket is not closed —
//! reads on it just block. Without a check, a session stays "connected" forever:
//! the schema tree, autocomplete, and the per-query database refresh keep firing
//! commands that queue behind the session mutex and never resolve, which is what
//! makes the app balloon in memory and stop responding after a connection loss.
//!
//! The heartbeat closes that gap: every [`HealthConfig::interval`] it pings each
//! live session (a trivial `SELECT 1` round-trip, bounded by [`PING_TIMEOUT`]).
//! A session that fails the ping is removed from [`AppState::sessions`] — which
//! drops and closes its connection — and a global `session:lost` event is
//! emitted so the frontend can tear the session down (clear the tree / detach
//! the tab) and offer a reconnect.
//!
//! Pinging is the only operation that touches a connection here, and it shares
//! the same session lock as queries, so the heartbeat never races a real query:
//! while a query holds the lock the heartbeat simply waits, and a connection
//! with a query in flight is, by definition, still alive.

use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};

use crate::error::IpcError;
use crate::state::{AppState, HealthConfig};

/// Floor for the configurable heartbeat interval. A sub-second interval would
/// spend the app's time pinging rather than working and buys nothing — a dropped
/// link is not an event you need millisecond latency on.
const MIN_INTERVAL_SECS: u64 = 2;

/// How long a single health-check ping may take before the session is treated
/// as dead. The heartbeat only pings *idle* sessions (a running query holds the
/// lock, deferring the ping), and an idle healthy connection answers `SELECT 1`
/// in milliseconds — so several seconds of silence reliably means the link is
/// gone, not merely slow. Kept comfortably above a normal round-trip to avoid
/// evicting a healthy session on a brief hiccup.
const PING_TIMEOUT: Duration = Duration::from_secs(8);

/// Payload for the global `session:lost` event. `camelCase` on the wire to match
/// the frontend's hand-written types.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLost {
    /// The session that was auto-closed.
    pub session_id: String,
    /// The connection it was opened from, so the UI can offer a reconnect.
    pub connection_id: String,
}

/// Update the heartbeat configuration at runtime.
///
/// The frontend calls this on startup and whenever the user changes the health
/// settings, so the running heartbeat task picks up the new interval / on-off
/// state on its next round without a restart. The interval is clamped to a sane
/// floor.
#[tauri::command]
pub async fn set_health_check(
    state: State<'_, AppState>,
    enabled: bool,
    interval_secs: u64,
) -> Result<(), IpcError> {
    let interval = Duration::from_secs(interval_secs.max(MIN_INTERVAL_SECS));
    *state.health.lock().expect("health config mutex poisoned") =
        HealthConfig { enabled, interval };
    tracing::info!(
        enabled,
        interval_secs = interval.as_secs(),
        "health check reconfigured"
    );
    Ok(())
}

/// Spawn the background heartbeat. Called once at startup with the app handle.
///
/// The loop reads the live [`HealthConfig`] each round, so enabling/disabling or
/// retuning it takes effect immediately. When disabled it just idles between
/// rounds; it never exits.
pub fn spawn_heartbeat<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        loop {
            let config = *app
                .state::<AppState>()
                .health
                .lock()
                .expect("health config mutex poisoned");

            if config.enabled {
                check_once(&app).await;
            }

            tokio::time::sleep(config.interval).await;
        }
    });
}

/// One heartbeat round: ping every live session and evict the dead ones.
///
/// Holds the sessions lock for the round so pings can't interleave with a query
/// on the same connection. Healthy pings are sub-millisecond, so for live
/// sessions the lock is held only briefly; a *dead* session costs up to
/// [`PING_TIMEOUT`] before it is evicted, which is the price of detecting it.
async fn check_once<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<AppState>();

    let lost = {
        let mut sessions = state.sessions.lock().await;
        let ids: Vec<String> = sessions.keys().cloned().collect();
        let mut lost: Vec<SessionLost> = Vec::new();

        for id in ids {
            let ping = match sessions.get_mut(&id) {
                Some(session) => tokio::time::timeout(PING_TIMEOUT, session.conn.ping()).await,
                None => continue,
            };
            // Either outcome — a ping error or a timeout — means the link is
            // gone. Evict the session (dropping it closes the connection).
            let dead = match ping {
                Ok(Ok(())) => false,
                Ok(Err(_)) | Err(_) => true,
            };
            if dead {
                if let Some(session) = sessions.remove(&id) {
                    tracing::warn!(session_id = %id, "health check failed; session auto-closed");
                    lost.push(SessionLost {
                        session_id: id,
                        connection_id: session.connection_id,
                    });
                }
            }
        }
        lost
    };

    // Notify the frontend after releasing the lock.
    for event in lost {
        let _ = app.emit("session:lost", event);
    }
}
