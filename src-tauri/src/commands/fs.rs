//! Local filesystem commands for file-backed query tabs.
//!
//! Selene tabs can be saved to / opened from `.sql` files anywhere on disk, and
//! a "workspace" can add folders whose `.sql` files show in the sidebar. These
//! commands are thin `std::fs` wrappers (no `selene-core` involvement — this is
//! not database logic). Tauri's capability/permission system gates only the JS
//! plugins, never Rust `std::fs`, so reading/writing user-chosen paths needs no
//! capability entry.
//!
//! ## Path canonicalization
//! Every path that crosses back to the frontend is canonicalized
//! ([`dunce::canonicalize`], which strips Windows `\\?\` UNC prefixes). The
//! frontend stores these canonical paths on tabs, and the file watcher (which
//! watches canonical roots, so the OS reports canonical child paths) emits the
//! same form — so a watcher event reliably matches the tab it belongs to.
//!
//! ## Logging discipline
//! File *contents* are never logged (same rule as SQL text); path-bearing logs
//! stay at `DEBUG`.

use std::fs;
use std::path::PathBuf;

use notify::{RecursiveMode, Watcher};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime, State};

use crate::error::IpcError;
use crate::state::AppState;

/// One entry in a listed directory: a subdirectory or a `.sql` file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsEntry {
    /// Final path component (file or directory name).
    pub name: String,
    /// Canonical absolute path.
    pub path: String,
    /// `true` for a directory, `false` for a `.sql` file.
    pub is_dir: bool,
}

/// A change to a watched file, emitted globally on the `"fs:change"` event.
///
/// Internally tagged (`kind`), `camelCase` fields — matching the `QueryEvent`
/// convention. The frontend reconciler matches `path` against open tabs.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FsEvent {
    /// A `.sql` file was created or modified on disk.
    #[serde(rename_all = "camelCase")]
    Changed { path: String },
    /// A `.sql` file was deleted or renamed away.
    #[serde(rename_all = "camelCase")]
    Removed { path: String },
}

/// Owns the live OS file watcher plus the set of canonical directory roots it
/// is recursively watching. Stored in [`AppState`] as `Mutex<Option<_>>`: it is
/// created lazily on the first [`fs_watch`] and dropped when the last root is
/// removed.
pub struct FsWatcher {
    watcher: notify::RecommendedWatcher,
    roots: Vec<PathBuf>,
}

// --- commands --------------------------------------------------------------

/// Read a UTF-8 text file's contents.
#[tauri::command]
pub async fn file_read(path: String) -> Result<String, IpcError> {
    read_file(&path)
}

/// Write `content` to `path` atomically (temp file in the same directory, then
/// rename over the target) and byte-faithfully (no newline normalization, so
/// the frontend's self-write suppression can compare disk bytes to its buffer).
#[tauri::command]
pub async fn file_write(path: String, content: String) -> Result<(), IpcError> {
    write_file(&path, &content)
}

/// List the immediate children of a directory: subdirectories and `.sql` files
/// only, dirs first then case-insensitive by name. Hidden entries (dotfiles,
/// `.git`, …) are skipped. Lazy — the sidebar tree calls this per expanded node.
#[tauri::command]
pub async fn dir_list(path: String) -> Result<Vec<FsEntry>, IpcError> {
    list_dir(&path)
}

/// Resolve a path to its canonical absolute form. Called right after the native
/// open dialogs so every path the frontend stores is in the same canonical form
/// the watcher reports.
#[tauri::command]
pub async fn canonicalize_path(path: String) -> Result<String, IpcError> {
    canonical_string(&path)
}

/// Start watching a directory recursively for `.sql` changes. Idempotent: a
/// root already watched is a no-op. Lazily creates the watcher on first call.
#[tauri::command]
pub async fn fs_watch<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    path: String,
) -> Result<(), IpcError> {
    let root = canonical(&path)?;
    let mut guard = state.watcher.lock().expect("watcher mutex poisoned");

    if guard.is_none() {
        // The handler runs on notify's own thread; `AppHandle` is Send + Sync,
        // so emitting from there is safe.
        let app_handle = app.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                emit_fs_event(&app_handle, &event);
            }
        })
        .map_err(|e| IpcError::new("io", format!("could not start file watcher: {e}")))?;
        *guard = Some(FsWatcher {
            watcher,
            roots: Vec::new(),
        });
    }

    let fw = guard.as_mut().expect("watcher present");
    if fw.roots.iter().any(|r| r == &root) {
        return Ok(());
    }
    fw.watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| IpcError::new("io", format!("could not watch '{}': {e}", root.display())))?;
    fw.roots.push(root);
    tracing::debug!("watching a folder for changes");
    Ok(())
}

/// Stop watching a directory. Removing the last root drops the whole watcher
/// (the next [`fs_watch`] recreates it). Unwatching an unknown root is a no-op.
#[tauri::command]
pub async fn fs_unwatch(state: State<'_, AppState>, path: String) -> Result<(), IpcError> {
    // Best-effort canonicalize: a removed directory can no longer be resolved,
    // so fall back to the raw path so the caller can still drop a stale root.
    let root = dunce::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
    let mut guard = state.watcher.lock().expect("watcher mutex poisoned");
    if let Some(fw) = guard.as_mut() {
        if let Some(pos) = fw.roots.iter().position(|r| r == &root) {
            let _ = fw.watcher.unwatch(&root);
            fw.roots.remove(pos);
        }
        if fw.roots.is_empty() {
            *guard = None;
        }
    }
    Ok(())
}

// --- sync helpers (unit-tested directly) -----------------------------------

/// Map an IO error to an [`IpcError`] with a stable `kind`. The path is
/// Selene-owned, secret-free text, safe to include in the message.
fn map_io(op: &str, path: &str, e: &std::io::Error) -> IpcError {
    let kind = match e.kind() {
        std::io::ErrorKind::NotFound => "not_found",
        std::io::ErrorKind::PermissionDenied => "permission_denied",
        _ => "io",
    };
    IpcError::new(kind, format!("could not {op} '{path}': {e}"))
}

fn canonical(path: &str) -> Result<PathBuf, IpcError> {
    dunce::canonicalize(path).map_err(|e| map_io("resolve", path, &e))
}

fn canonical_string(path: &str) -> Result<String, IpcError> {
    Ok(canonical(path)?.to_string_lossy().into_owned())
}

fn read_file(path: &str) -> Result<String, IpcError> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        // `read_to_string` reports non-UTF-8 as `InvalidData`; give it a clear,
        // distinct kind rather than a generic IO error.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Err(IpcError::new(
            "invalid_utf8",
            format!("'{path}' is not valid UTF-8 text"),
        )),
        Err(e) => Err(map_io("read", path, &e)),
    }
}

fn write_file(path: &str, content: &str) -> Result<(), IpcError> {
    let target = PathBuf::from(path);
    let parent = target
        .parent()
        .ok_or_else(|| IpcError::new("io", format!("invalid file path '{path}'")))?;
    let file_name = target
        .file_name()
        .ok_or_else(|| IpcError::new("io", format!("invalid file path '{path}'")))?;

    // Sibling temp file (hidden, so a racing `dir_list` skips it), then rename
    // over the target so a crash mid-write cannot truncate the file.
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(".selene-tmp");
    let tmp = parent.join(tmp_name);

    if let Err(e) = fs::write(&tmp, content.as_bytes()) {
        return Err(map_io("write", path, &e));
    }
    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(map_io("save", path, &e));
    }
    tracing::debug!("wrote a file to disk");
    Ok(())
}

fn list_dir(path: &str) -> Result<Vec<FsEntry>, IpcError> {
    let dir = canonical(path)?;
    let read = fs::read_dir(&dir).map_err(|e| map_io("read directory", path, &e))?;

    let mut entries = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip hidden entries (.git, .vscode, dotfiles): noise for a SQL browser.
        if name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        let is_sql = !is_dir && name.to_ascii_lowercase().ends_with(".sql");
        if !is_dir && !is_sql {
            continue;
        }
        entries.push(FsEntry {
            name,
            path: entry.path().to_string_lossy().into_owned(),
            is_dir,
        });
    }

    // Directories first, then files; each group case-insensitive by name.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a
            .name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase()),
    });
    Ok(entries)
}

/// Translate one OS watch event into zero or more [`FsEvent`]s. Only `.sql`
/// paths are forwarded (a recursive folder watch would otherwise fire on `.git`
/// churn, builds, etc.); the frontend decides whether each path is open or in a
/// listed folder.
fn emit_fs_event<R: Runtime>(app: &AppHandle<R>, event: &notify::Event) {
    use notify::EventKind;
    let removed = matches!(event.kind, EventKind::Remove(_));
    let changed = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_));
    if !removed && !changed {
        return;
    }
    for path in &event.paths {
        let is_sql = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("sql"));
        if !is_sql {
            continue;
        }
        let path = path.to_string_lossy().into_owned();
        let payload = if removed {
            FsEvent::Removed { path }
        } else {
            FsEvent::Changed { path }
        };
        let _ = app.emit("fs:change", payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn write_then_read_round_trips_byte_faithfully() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("query.sql");
        let path_str = path.to_string_lossy().to_string();

        // Mixed line endings + unicode must survive verbatim (the self-write
        // suppression compares disk bytes to the editor buffer).
        let content = "SELECT 1\r\nWHERE name = N'café';\n-- 漢字\n";
        write_file(&path_str, content).unwrap();
        assert_eq!(read_file(&path_str).unwrap(), content);
    }

    #[test]
    fn write_is_atomic_and_leaves_no_temp_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("q.sql");
        write_file(&path.to_string_lossy(), "SELECT 1;").unwrap();

        // Only the target exists — the sibling temp was renamed away.
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["q.sql".to_string()]);
    }

    #[test]
    fn read_missing_file_is_not_found() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope.sql");
        let err = read_file(&missing.to_string_lossy()).unwrap_err();
        assert_eq!(err.kind, "not_found");
    }

    #[test]
    fn read_non_utf8_is_reported_distinctly() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("binary.sql");
        fs::write(&path, [0xff, 0xfe, 0x00, 0x9f]).unwrap();
        let err = read_file(&path.to_string_lossy()).unwrap_err();
        assert_eq!(err.kind, "invalid_utf8");
    }

    #[test]
    fn dir_list_keeps_dirs_and_sql_sorted_dirs_first() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("b.sql"), "").unwrap();
        fs::write(root.join("a.sql"), "").unwrap();
        fs::write(root.join("notes.txt"), "").unwrap(); // filtered out
        fs::write(root.join(".hidden.sql"), "").unwrap(); // hidden, filtered
        fs::create_dir(root.join("sub")).unwrap();
        fs::create_dir(root.join(".git")).unwrap(); // hidden dir, filtered

        let entries = list_dir(&root.to_string_lossy()).unwrap();
        let shape: Vec<(&str, bool)> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.is_dir))
            .collect();
        assert_eq!(
            shape,
            vec![("sub", true), ("a.sql", false), ("b.sql", false)]
        );
    }

    #[test]
    fn canonical_string_resolves_an_existing_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("q.sql");
        fs::write(&path, "").unwrap();
        let resolved = canonical_string(&path.to_string_lossy()).unwrap();
        // Resolved form ends with the file name and is absolute.
        assert!(resolved.ends_with("q.sql"));
        assert!(PathBuf::from(&resolved).is_absolute());
    }
}
