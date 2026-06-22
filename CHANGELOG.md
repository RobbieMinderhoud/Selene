# Changelog

All notable **functional** (user-facing) changes to Selene are documented here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Internal refactors, tooling, and chores are intentionally omitted. (Automated
changelog generation from Conventional Commits via git-cliff is planned but not
yet set up.)

## [1.2.1] - 2026-06-22

### Changed

- **Renaming a database no longer hangs when the database is in use.** The rename
  now fails fast (a short lock timeout) instead of blocking indefinitely, and when
  the database has active connections Selene asks whether to **force the rename** —
  which disconnects those sessions (rolling back any in-flight transactions) and
  completes the rename.

### Added

- The schema tree now shows an **inline status** on a database row while a
  management operation runs — a spinner plus a label such as "renaming…", "taking
  offline…", "bringing online…", "dropping…", or "creating…" — so these
  operations are no longer silent.

## [1.2.0] - 2026-06-22

### Added

- Database management from the schema tree's right-click menu: **create** a new
  database (right-click the connection row → "New database…", inline name entry),
  **drop** a database (you must re-type the database name to confirm — the action
  is permanent), **rename** a database (inline edit in the tree), **take it
  offline** (with a confirmation — this immediately drops all connections to it),
  and **bring it back online**. Offline databases stay listed (shown muted with an
  "OFFLINE" badge) so they can be brought back. Drop/rename/offline are disabled
  for system databases, and all of these are disabled on read-only connections.
- **Type-to-filter** for the Connections, Files, and Schema panels: with a panel
  focused, just start typing to filter it — no search box. A small chip shows the
  current filter; Backspace edits it and Escape clears it. Filtering is shallow
  (it matches what's already loaded plus the top level; expanded nodes stay open).

## [1.1.0] - 2026-06-22

### Added

- Automatic detection and close-out of dropped connections. A background health
  check periodically pings every open connection; one that stops responding
  (e.g. after Wi-Fi/VPN loss or the machine sleeping) is closed automatically
  instead of leaving the app spinning and consuming memory on requests that can
  never complete. Each affected editor tab shows a "Disconnected" status with a
  one-click **Reconnect**, and a toast reports the loss. The check (on/off and
  interval) is configurable in Settings → General → Connection health.
- Running a query on a tab whose connection has dropped now reconnects
  automatically and restores the database the tab was last using (e.g. after a
  `USE`), then runs — instead of leaving the Run button disabled. The manual
  Reconnect also restores the last database.

### Changed

- Connections now use a bounded connect timeout and TCP keepalive, so an
  unreachable or silently dropped server fails promptly rather than hanging.

## [1.0.1] - 2026-06-22

### Fixed

- High CPU, GPU, and battery usage while a connection was open. The pulsing
  "connected" indicator in the connection list animated a paint-heavy property,
  which forced the whole window to repaint on every frame and kept the app busy
  even when idle. It now animates cheaply, so an idle connected window no longer
  drains power.

## [1.0.0] - 2026-06-19

First public release. The `selene-core` data layer, the Tauri IPC layer, and the
desktop UI are all implemented; remaining work (broader E2E coverage, multi-OS
packaging, signed releases) is tracked on the roadmap.

### Added

- Connect to Microsoft SQL Server using SQL Server authentication (username/password).
- Save and manage connections; passwords are stored in the OS keychain and never
  written to disk or logs.
- Prompt for a password inline when connecting to a connection that has none
  stored (e.g. an imported connection, or one whose keychain entry was removed)
  instead of failing — re-entering it reconnects and saves it for next time.
- Run SQL queries with results streamed incrementally into the app.
- Cancel a running query.
- Browse the database object tree: databases → schemas → tables/views → columns.
- Export query results to CSV, JSON, and Excel (XLSX).
- Import a CSV file into the database. Right-click a table in the schema tree to
  import into it (mapping CSV columns onto the table's columns, auto-matched by
  name), or right-click a schema to import as a new table (Selene infers a SQL
  type per column from the data, all editable). A mapping menu previews the file
  and lets you adjust the delimiter/quote/header, choose how empty fields map to
  NULL, and pick whether a bad row aborts the whole import (transactional) or is
  skipped and reported. Progress streams into a toast and the tree refreshes when
  it finishes. Importing is disabled on read-only connections. Defaults for the
  parse options live in Settings → Import.
- A SQL safety guard that warns before destructive statements (UPDATE/DELETE
  without a WHERE clause, DROP, TRUNCATE, …) and a per-connection read-only mode
  that blocks non-SELECT statements.
- Opening a SQL file auto-connects its tab to the saved connection whose name
  appears in the file name (e.g. `pr02db02b_shared_01.sql` connects to
  `pr02db02b`). Matching is case-insensitive and token-bounded, and the most
  specific name wins when several match; a tab that is already connected is never
  overridden. Can be turned off via Settings → Query behaviour → "Auto-connect
  opened files by name".
- Find & replace in the editor (Cmd/Ctrl+F to find, Cmd/Ctrl+Alt+F for replace),
  with live match counts, next/previous navigation, and match highlighting.
  Supports case-sensitive, whole-word, and **regular-expression** search (invalid
  patterns are flagged). The find box seeds from the current selection, Enter
  finds the next match (Shift+Enter the previous), and your case/word/regex toggle
  choices are remembered between sessions.
- A rollback-wrapped dry-run (`BEGIN TRAN; <UPDATE/DELETE/INSERT …>; ROLLBACK`)
  now labels its result "Rolled back · N rows affected", so it is clear the row
  counts are what *would* have changed and that nothing was committed.
- The results status bar and the "rows affected" message are now selectable, so
  the row count (and elapsed time) can be copied.
- Open local folders into a sidebar file tree and open `.sql` files into editor
  tabs. File-backed tabs save to disk (Cmd/Ctrl+S, and on tab switch / window
  blur) and stay in sync with external edits: a clean tab silently reloads when
  the file changes on disk, while a tab with unsaved changes prompts you to keep
  your version or reload from disk. Open folders and tabs are restored next launch.
- Schema-aware autocomplete in the editor: table names complete eagerly and a
  table's columns complete on first reference (fetched lazily per connection).
  Can be turned off via Settings → Editor.
- Switch the active database per tab from a toolbar database selector; it runs
  `USE [db]` on that tab's own session, so tabs don't interfere with each other.
- A settings screen for appearance, editor, results, query behaviour, and CSV
  export/import defaults — plus export/import of saved connections (without
  passwords) and backup/restore of all settings.

### Changed

- SQL files open last session are no longer reopened on launch; the editor now
  starts clean each session. Workspace folders are still restored, so any file
  can be reopened from the sidebar tree.
- The interface now animates throughout for a smoother, less static feel:
  dialogs and notifications ease in and out, editor tabs and result-set tabs
  glide, the schema tree and connection list reveal their contents, query
  results fade in, and loading states show a spinner. Switching between light
  and dark themes crossfades. All motion is subtle and fast, and fully respects
  the OS "reduce motion" accessibility setting.

### Fixed

- A batch that begins with `USE <database>` and then modifies data (e.g.
  `USE web02; INSERT …; INSERT …`) now reports each statement's affected-row
  count instead of showing empty or missing result tabs. The `USE` switches the
  active database (with a "Switched to database …" notice) and the remaining
  statements report their counts.
- Affected-row counts now appear for a rolled-back dry-run even when
  `BEGIN TRANSACTION` is written on its own line without a trailing semicolon
  (e.g. `BEGIN TRANSACTION` ⏎ `INSERT …` ⏎ `ROLLBACK`).

### Security

- Connections use TLS by default; skipping certificate validation is an explicit,
  per-connection opt-in.
