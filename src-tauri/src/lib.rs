//! The Selene desktop shell: a thin Tauri 2 IPC adapter over `selene-core`.
//!
//! This crate owns no database, export, or guard logic — it (de)serializes
//! command arguments, holds the live application state ([`state::AppState`]),
//! and forwards work to `selene-core`. All result data streams to the frontend
//! over `tauri::ipc::Channel`s; see [`commands`] for the wire contract.
//!
//! ## Logging & secrets
//! Logging is configured via `tauri-plugin-log` (a global `log` logger at
//! `INFO`); `tracing` events fall through to it (the `tracing/log` feature).
//! Command instrumentation never logs SQL text, row data, or secrets above
//! `DEBUG`/`TRACE`, and [`Secret`](selene_core::Secret) cannot be serialized or
//! `Display`ed at all.

#![forbid(unsafe_code)]

mod commands;
mod error;
mod state;

pub use error::IpcError;

use tauri::{Emitter, Manager};
use tauri_plugin_log::{Target, TargetKind};

use state::AppState;

/// Build and run the Tauri application.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Native save-file picker for the export flow.
        .plugin(tauri_plugin_dialog::init())
        // Logging sinks: stdout (dev), the per-OS log directory (support), and
        // the webview console (in-app diagnostics). INFO by default; `tracing`
        // events flow here via the `tracing/log` feature. SQL text, row data,
        // and secrets are never logged above DEBUG/TRACE by command code.
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(tauri_plugin_log::log::LevelFilter::Info)
                .targets([
                    Target::new(TargetKind::Stdout),
                    Target::new(TargetKind::LogDir { file_name: None }),
                    Target::new(TargetKind::Webview),
                ])
                .build(),
        )
        .setup(|app| {
            // Build the macOS native menu. Menu items emit Tauri events the
            // frontend listens for (Settings ⌘,, and the File menu's New/Open/
            // Save commands). The rest are the standard macOS Application and
            // Edit menus. Windows/Linux have no native menu yet, so there the
            // same commands are bound via a window-level key handler (see the
            // frontend's App component) — keeping macOS on the menu accelerators
            // alone avoids a double-fire.
            #[cfg(target_os = "macos")]
            {
                use tauri::menu::{MenuBuilder, MenuItem, SubmenuBuilder};

                let settings = MenuItem::with_id(
                    app,
                    "settings",
                    "Settings\u{2026}", // "Settings…"
                    true,
                    Some("CmdOrCtrl+,"),
                )?;

                let app_menu = SubmenuBuilder::new(app, "Selene")
                    .about(None)
                    .separator()
                    .item(&settings)
                    .separator()
                    .services()
                    .separator()
                    .hide()
                    .hide_others()
                    .show_all()
                    .separator()
                    .quit()
                    .build()?;

                let close_tab =
                    MenuItem::with_id(app, "close-tab", "Close Tab", true, Some("CmdOrCtrl+W"))?;
                let new_query =
                    MenuItem::with_id(app, "new-query", "New Query", true, Some("CmdOrCtrl+N"))?;
                let open_file = MenuItem::with_id(
                    app,
                    "open-file",
                    "Open File\u{2026}",
                    true,
                    Some("CmdOrCtrl+O"),
                )?;
                let open_folder = MenuItem::with_id(
                    app,
                    "open-folder",
                    "Open Folder\u{2026}",
                    true,
                    Some("CmdOrCtrl+Shift+O"),
                )?;
                let save = MenuItem::with_id(app, "save", "Save", true, Some("CmdOrCtrl+S"))?;
                let save_as = MenuItem::with_id(
                    app,
                    "save-as",
                    "Save As\u{2026}",
                    true,
                    Some("CmdOrCtrl+Shift+S"),
                )?;

                let file_menu = SubmenuBuilder::new(app, "File")
                    .item(&new_query)
                    .item(&close_tab)
                    .separator()
                    .item(&open_file)
                    .item(&open_folder)
                    .separator()
                    .item(&save)
                    .item(&save_as)
                    .build()?;

                let edit_menu = SubmenuBuilder::new(app, "Edit")
                    .undo()
                    .redo()
                    .separator()
                    .cut()
                    .copy()
                    .paste()
                    .separator()
                    .select_all()
                    .build()?;

                let menu = MenuBuilder::new(app)
                    .item(&app_menu)
                    .item(&file_menu)
                    .item(&edit_menu)
                    .build()?;

                app.set_menu(menu)?;
                app.on_menu_event(|app, event| {
                    let id = event.id();
                    let emit = if id == "settings" {
                        Some("menu:open-settings")
                    } else if id == "new-query" {
                        Some("menu:new-query")
                    } else if id == "open-file" {
                        Some("menu:open-file")
                    } else if id == "open-folder" {
                        Some("menu:open-folder")
                    } else if id == "save" {
                        Some("menu:save")
                    } else if id == "save-as" {
                        Some("menu:save-as")
                    } else if id == "close-tab" {
                        Some("menu:close-tab")
                    } else {
                        None
                    };
                    if let Some(emit) = emit {
                        let _ = app.emit(emit, ());
                    }
                });
            }

            // Resolve the app config dir and build shared state. A failure here
            // is fatal (the app cannot persist connections), so it aborts setup
            // with a context-rich error.
            let config_dir = app
                .path()
                .app_config_dir()
                .map_err(|e| format!("could not resolve app config directory: {e}"))?;
            let app_state = AppState::new(config_dir)
                .map_err(|e| format!("could not initialise app state: {e}"))?;
            app.manage(app_state);
            tracing::info!(version = selene_core::VERSION, "Selene backend initialised");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // Connections
            commands::connection::connections_list,
            commands::connection::connection_save,
            commands::connection::connection_delete,
            commands::connection::connection_reorder,
            commands::connection::connection_test,
            commands::connection::connections_import,
            // Sessions
            commands::session::session_connect,
            commands::session::session_disconnect,
            commands::session::session_use_database,
            commands::session::session_current_database,
            // Introspection
            commands::introspect::databases_list,
            commands::introspect::schemas_list,
            commands::introspect::tables_list,
            commands::introspect::columns_list,
            // Query + guard
            commands::query::guard_check,
            commands::query::query_run,
            commands::query::query_cancel,
            // Export
            commands::export::export_result,
            // Import
            commands::import::import_csv_analyze,
            commands::import::import_csv,
            // Filesystem (file-backed tabs + workspace folders)
            commands::fs::file_read,
            commands::fs::file_write,
            commands::fs::dir_list,
            commands::fs::canonicalize_path,
            commands::fs::fs_watch,
            commands::fs::fs_unwatch,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
