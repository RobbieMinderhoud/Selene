//! Application state shared across all IPC commands.
//!
//! Three pieces of state live for the lifetime of the app, behind a single
//! [`AppState`] handed to Tauri via `.manage(...)` and pulled into commands as
//! `State<'_, AppState>`:
//!
//! - [`ConnectionStore`] — the persisted list of (non-secret)
//!   [`ConnectionSpec`]s, serialized to `connections.json` in the OS app-config
//!   directory.
//! - [`KeychainStore`] — the OS keychain wrapper from `selene-core`; the *only*
//!   place a connection password is read or written.
//! - the live sessions map and the running-query token map.
//!
//! ## Concurrency model (v0.1)
//! Each connected session owns exactly one live [`Connection`], guarded by a
//! `tokio::sync::Mutex`. Two queries issued against the *same* session
//! therefore serialize behind that mutex — a deliberate v0.1 simplification (a
//! per-connection pool that runs them in parallel is deferred). Queries on
//! *different* sessions run concurrently. The async mutex (not `std::sync`) is
//! required because the lock is held across `.await` points while a query
//! streams.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Mutex as AsyncMutex, Notify};

use selene_core::{
    CancelToken, Connection, ConnectionSpec, CoreError, DriverCapabilities, DriverId,
    KeychainStore, Secret,
};

/// Pause/resume coordination for one in-flight multi-target run.
///
/// A multi-target run fans out across servers in parallel; each server task
/// parks at this gate before starting its next database. When the run's failure
/// rate crosses the configured threshold, exactly one task flips the gate
/// ([`try_pause`](Self::try_pause)) so the whole run idles until the user
/// resumes ([`resume`](Self::resume)) or cancels (which calls
/// [`wake`](Self::wake) so parked tasks observe the cancel token and exit).
///
/// The gate triggers **at most once per run**: `try_pause` is a one-shot, so a
/// run that the user chose to continue is never paused again.
#[derive(Default)]
pub struct PauseGate {
    /// Whether the run is currently parked, waiting for a user decision.
    paused: AtomicBool,
    /// One-shot latch so the gate ever pauses at most once per run.
    triggered: AtomicBool,
    /// Wakes tasks parked in [`wait_while_paused`](Self::wait_while_paused).
    notify: Notify,
}

impl PauseGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to be the single task that pauses this run. Returns `true` exactly
    /// once across all callers (and never again after a resume), so the caller
    /// that wins is the one that should emit the `Paused` event.
    pub fn try_pause(&self) -> bool {
        if self
            .triggered
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.paused.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Whether the run is currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Clear the pause and wake parked tasks so they continue (user chose
    /// "Continue").
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Wake parked tasks **without** clearing the pause. Used on cancel: the
    /// woken tasks re-check the cancel token and exit.
    pub fn wake(&self) {
        self.notify.notify_waiters();
    }

    /// Park until the run is resumed or cancelled. A no-op when not paused.
    pub async fn wait_while_paused(&self, cancel: &CancelToken) {
        loop {
            if cancel.is_cancelled() || !self.is_paused() {
                return;
            }
            // Arm the notification *before* the final re-check so a resume/wake
            // landing in this window is not lost.
            let notified = self.notify.notified();
            if cancel.is_cancelled() || !self.is_paused() {
                return;
            }
            notified.await;
        }
    }
}

/// File name for the persisted connection list, under the app config dir.
const CONNECTIONS_FILE: &str = "connections.json";

/// Persists the (non-secret) list of connection specs to a JSON file in the
/// app config directory.
///
/// Secrets never touch this file — they live only in the OS keychain
/// ([`KeychainStore`]). The store is intentionally simple: the whole list is
/// read or rewritten on each mutation, which is more than fast enough for the
/// handful of connections a desktop user keeps.
pub struct ConnectionStore {
    /// Absolute path to `connections.json`.
    path: PathBuf,
}

impl ConnectionStore {
    /// Create a store rooted at `config_dir`, creating the directory if it does
    /// not yet exist. `config_dir` is the app config dir resolved via
    /// `AppHandle::path().app_config_dir()`.
    pub fn new(config_dir: PathBuf) -> Result<Self, CoreError> {
        fs::create_dir_all(&config_dir)
            .map_err(|e| CoreError::Io(format!("could not create app config directory: {e}")))?;
        Ok(Self {
            path: config_dir.join(CONNECTIONS_FILE),
        })
    }

    /// Read and parse the persisted list. A missing file is an empty list, not
    /// an error (the first run has no saved connections).
    fn read_all(&self) -> Result<Vec<ConnectionSpec>, CoreError> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| CoreError::Config(format!("could not parse {CONNECTIONS_FILE}: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(CoreError::Io(format!(
                "could not read {CONNECTIONS_FILE}: {e}"
            ))),
        }
    }

    /// Atomically-enough rewrite the whole list (write a temp file, then
    /// rename over the target so a crash mid-write cannot truncate the list).
    fn write_all(&self, specs: &[ConnectionSpec]) -> Result<(), CoreError> {
        let json = serde_json::to_vec_pretty(specs)
            .map_err(|e| CoreError::Config(format!("could not serialize connections: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &json)
            .map_err(|e| CoreError::Io(format!("could not write {CONNECTIONS_FILE}: {e}")))?;
        fs::rename(&tmp, &self.path)
            .map_err(|e| CoreError::Io(format!("could not commit {CONNECTIONS_FILE}: {e}")))
    }

    /// All saved connection specs, in file order.
    pub fn list(&self) -> Result<Vec<ConnectionSpec>, CoreError> {
        self.read_all()
    }

    /// Fetch one spec by id, if present.
    pub fn get(&self, id: &str) -> Result<Option<ConnectionSpec>, CoreError> {
        Ok(self.read_all()?.into_iter().find(|s| s.id == id))
    }

    /// Insert `spec`, or replace the existing entry with the same id. Returns
    /// the stored spec (unchanged from the input; returned for caller
    /// convenience).
    pub fn upsert(&self, spec: ConnectionSpec) -> Result<ConnectionSpec, CoreError> {
        let mut all = self.read_all()?;
        match all.iter_mut().find(|s| s.id == spec.id) {
            Some(existing) => *existing = spec.clone(),
            None => all.push(spec.clone()),
        }
        self.write_all(&all)?;
        Ok(spec)
    }

    /// Rewrite the stored list in the order given by `ids`.
    ///
    /// Ids present in `ids` but absent from the store are silently skipped.
    /// Stored entries whose id does not appear in `ids` are appended at the end
    /// so nothing is ever silently dropped.
    pub fn reorder(&self, ids: &[String]) -> Result<(), CoreError> {
        let mut all = self.read_all()?;
        let pos: std::collections::HashMap<&str, usize> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        all.sort_by_key(|s| pos.get(s.id.as_str()).copied().unwrap_or(usize::MAX));
        self.write_all(&all)
    }

    /// Remove the spec with `id`. Removing an absent id is a no-op (idempotent).
    pub fn delete(&self, id: &str) -> Result<(), CoreError> {
        let mut all = self.read_all()?;
        let before = all.len();
        all.retain(|s| s.id != id);
        if all.len() != before {
            self.write_all(&all)?;
        }
        Ok(())
    }
}

/// One live, connected session: a single driver connection plus the metadata a
/// command needs without re-reading the store.
///
/// `driver`, `caps`, and `connection_id` are snapshotted at connect time and
/// returned to the frontend via [`SessionInfo`](crate::commands::session::SessionInfo).
/// `driver` is read by `query_run` to pick the safety guard; `caps` and
/// `connection_id` are retained for diagnostics and for forthcoming
/// session-scoped commands (e.g. a reconnect that re-reads the originating spec,
/// or capability-gated behaviour) — hence `#[allow(dead_code)]` on those two
/// rather than dropping them from the documented session model.
pub struct SessionEntry {
    /// Which backend this session is connected to. Read by `query_run` to select
    /// the safety guard (SQL vs. MongoDB) for server-side enforcement.
    pub driver: DriverId,
    /// The driver's capabilities, snapshotted at connect time.
    #[allow(dead_code)]
    pub caps: DriverCapabilities,
    /// The saved connection this session was opened from, so the session can be
    /// matched back to its config (reconnect, diagnostics).
    #[allow(dead_code)]
    pub connection_id: String,
    /// Cached read-only flag from the connection spec at connect time, so the
    /// SQL guard can be enforced without re-reading the store on every query.
    pub read_only: bool,
    /// The live connection. Guarded so it is driven from one task at a time.
    pub conn: Box<dyn Connection>,
}

/// Runtime configuration for the connection health-check heartbeat.
///
/// The heartbeat task (see `commands::health`) reads this each round, so the
/// frontend can enable/disable it or retune the interval live via
/// `set_health_check` without a restart. Defaults match the frontend's settings
/// defaults (enabled, every 5s).
#[derive(Debug, Clone, Copy)]
pub struct HealthConfig {
    /// Whether the heartbeat actively pings live sessions.
    pub enabled: bool,
    /// Delay between heartbeat rounds.
    pub interval: Duration,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(5),
        }
    }
}

/// The whole application's shared state.
///
/// `AppState` is `Send + Sync` (every field is), as required by Tauri's
/// `State<'_, T>`. `Box<dyn Connection>` is only `Send`, but it lives behind a
/// `tokio::sync::Mutex`, which is `Sync` whenever its contents are `Send`.
pub struct AppState {
    /// Persisted, non-secret connection specs.
    pub store: ConnectionStore,
    /// OS keychain wrapper (the only secret store).
    pub keychain: KeychainStore,
    /// In-process cache of connection passwords already unlocked from the OS
    /// keychain this run, keyed by `connection_id`. The first connect (or test)
    /// for a connection reads the keychain — which, on an unsigned/dev macOS
    /// build, is what triggers the system "allow access" prompt — then memoizes
    /// the value, so reconnecting the same connection in one session neither
    /// re-reads the keychain nor re-prompts. Kept in step with the store:
    /// updated on `connection_save`, removed on `connection_delete`.
    ///
    /// Security tradeoff: a cached `Secret` stays resident for the process
    /// lifetime rather than being dropped right after each connect. It remains a
    /// `Secret` (redacted `Debug`, zeroized on drop/replace) and is never
    /// persisted or logged; only the in-memory window is longer — acceptable for
    /// a credential the user has already chosen to unlock this session.
    ///
    /// Plain `std::sync::Mutex`: locked only briefly to read, insert, or remove
    /// a cloned `Secret`, never across an `.await`.
    pub secret_cache: Mutex<HashMap<String, Secret>>,
    /// Live sessions keyed by `session_id`.
    pub sessions: AsyncMutex<HashMap<String, SessionEntry>>,
    /// Cancellation tokens for in-flight queries, keyed by `query_id`. A plain
    /// `std::sync::Mutex` suffices: it is only ever locked briefly to insert,
    /// look up, or remove a cheap clone of a token, never across `.await`.
    pub running: Mutex<HashMap<String, CancelToken>>,
    /// Pause gates for in-flight multi-target runs, keyed by `run_id`, so
    /// `multi_target_resume`/`multi_target_cancel` can wake parked server tasks.
    /// Inserted in `multi_target_run`, removed when the run ends. A plain
    /// `Mutex`: locked only briefly to insert, clone out, or remove a handle.
    pub multi_gates: Mutex<HashMap<String, Arc<PauseGate>>>,
    /// The live filesystem watcher for file-backed tabs / workspace folders.
    /// Created lazily on the first `fs_watch` and dropped when no roots remain;
    /// a plain `Mutex` suffices (locked only briefly to add/remove a root).
    pub watcher: Mutex<Option<crate::commands::fs::FsWatcher>>,
    /// Live tuning for the connection health-check heartbeat. A plain `Mutex`:
    /// read once per heartbeat round, written only by `set_health_check`.
    pub health: Mutex<HealthConfig>,
}

impl AppState {
    /// Build the application state. `config_dir` is the resolved app config
    /// directory; the connection store is rooted there.
    pub fn new(config_dir: PathBuf) -> Result<Self, CoreError> {
        Ok(Self {
            store: ConnectionStore::new(config_dir)?,
            keychain: KeychainStore::new(),
            secret_cache: Mutex::new(HashMap::new()),
            sessions: AsyncMutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            multi_gates: Mutex::new(HashMap::new()),
            watcher: Mutex::new(None),
            health: Mutex::new(HealthConfig::default()),
        })
    }

    /// Read a connection password through [`secret_cache`](Self::secret_cache).
    ///
    /// Returns the cached value when this connection was already unlocked this
    /// session; otherwise reads the OS keychain (the call that may prompt on a
    /// dev build), memoizes a clone, and returns it. `Ok(None)` means no secret
    /// is stored for `connection_id`.
    pub fn cached_secret(&self, connection_id: &str) -> Result<Option<Secret>, CoreError> {
        if let Some(secret) = self
            .secret_cache
            .lock()
            .expect("secret cache mutex poisoned")
            .get(connection_id)
        {
            return Ok(Some(secret.clone()));
        }
        match self.keychain.get_secret(connection_id)? {
            Some(secret) => {
                self.secret_cache
                    .lock()
                    .expect("secret cache mutex poisoned")
                    .insert(connection_id.to_string(), secret.clone());
                Ok(Some(secret))
            }
            None => Ok(None),
        }
    }

    /// Insert/replace the cached password for `connection_id` after the stored
    /// secret has just been (re)written, so the next connect uses the new value
    /// without a keychain round-trip or prompt.
    pub fn cache_secret(&self, connection_id: &str, secret: Secret) {
        self.secret_cache
            .lock()
            .expect("secret cache mutex poisoned")
            .insert(connection_id.to_string(), secret);
    }

    /// Drop any cached password for `connection_id` (on delete, or whenever the
    /// stored value may have changed out from under the cache).
    pub fn invalidate_cached_secret(&self, connection_id: &str) {
        self.secret_cache
            .lock()
            .expect("secret cache mutex poisoned")
            .remove(connection_id);
    }
}

/// Generate a fresh, opaque, collision-resistant id (UUID v4) for a connection,
/// session, or running query.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use selene_core::AuthMethod;
    use tempfile::TempDir;

    fn spec(id: &str, name: &str) -> ConnectionSpec {
        ConnectionSpec {
            id: id.to_string(),
            name: name.to_string(),
            driver: DriverId::Mssql,
            host: "localhost".to_string(),
            port: None,
            instance: None,
            uri: None,
            database: None,
            auth: AuthMethod::SqlLogin {
                username: "sa".to_string(),
            },
            tls: Default::default(),
            read_only: false,
        }
    }

    #[test]
    fn missing_file_lists_empty() {
        let dir = TempDir::new().unwrap();
        let store = ConnectionStore::new(dir.path().to_path_buf()).unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(store.get("nope").unwrap().is_none());
    }

    #[test]
    fn upsert_then_list_and_get_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = ConnectionStore::new(dir.path().to_path_buf()).unwrap();

        store.upsert(spec("a", "Alpha")).unwrap();
        store.upsert(spec("b", "Beta")).unwrap();

        let all = store.list().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(store.get("a").unwrap().unwrap().name, "Alpha");

        // Re-reading from a fresh store proves it persisted to disk.
        let reopened = ConnectionStore::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(reopened.list().unwrap().len(), 2);
    }

    #[test]
    fn upsert_replaces_same_id() {
        let dir = TempDir::new().unwrap();
        let store = ConnectionStore::new(dir.path().to_path_buf()).unwrap();

        store.upsert(spec("a", "Alpha")).unwrap();
        store.upsert(spec("a", "Renamed")).unwrap();

        let all = store.list().unwrap();
        assert_eq!(all.len(), 1, "same id must replace, not duplicate");
        assert_eq!(all[0].name, "Renamed");
    }

    #[test]
    fn delete_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let store = ConnectionStore::new(dir.path().to_path_buf()).unwrap();

        store.upsert(spec("a", "Alpha")).unwrap();
        store.delete("a").unwrap();
        assert!(store.list().unwrap().is_empty());
        // Deleting again is a no-op.
        store.delete("a").unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn new_id_is_unique() {
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn secret_cache_hits_updates_and_invalidates() {
        use selene_core::Secret;

        let dir = TempDir::new().unwrap();
        let state = AppState::new(dir.path().to_path_buf()).unwrap();

        // Prime the cache directly: the read-through slow path would touch the
        // real OS keychain, which these hermetic tests must never do.
        state.cache_secret("c1", Secret::new("pw"));

        // Fast path returns the cached secret with no keychain read (no prompt).
        let got = state.cached_secret("c1").unwrap().expect("cached secret");
        assert_eq!(got.expose(), "pw");

        // A save-time update replaces the value in place.
        state.cache_secret("c1", Secret::new("pw2"));
        assert_eq!(state.cached_secret("c1").unwrap().unwrap().expose(), "pw2");

        // Invalidation removes it (a subsequent read would fall through to the
        // keychain — not exercised here to keep the test off the real store).
        state.invalidate_cached_secret("c1");
        assert!(state
            .secret_cache
            .lock()
            .expect("secret cache mutex poisoned")
            .get("c1")
            .is_none());
    }
}
