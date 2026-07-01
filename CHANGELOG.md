# Changelog

All notable **functional** (user-facing) changes to Selene are documented here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Internal refactors, tooling, and chores are intentionally omitted. (Automated
changelog generation from Conventional Commits via git-cliff is planned but not
yet set up.)

## [1.4.0] - 2026-07-01

### Added

- **Connect to MongoDB.** Pick MongoDB in the connection dialog (host/port or a
  `mongodb://` / `mongodb+srv://` URI), browse databases and collections with
  sampled field shapes, and run mongosh-style read queries
  (`db.coll.find(...)`, `.aggregate([...])`, `countDocuments`, `distinct`) with
  results — including nested documents/arrays — shown in the grid. Read-only
  connections block writes.
- **Connect to PostgreSQL, MySQL, and SQLite, not just SQL Server.** The
  connection dialog now starts with a **Driver** picker; choose the backend and
  the form adapts — PostgreSQL and MySQL show host/port (defaulting to 5432 /
  3306) without the SQL-Server-only named-instance field, while SQLite asks for
  a single **Database file** (with a **Browse…** button) and no host, port, or
  login. The editor highlights and auto-completes keywords for whichever backend
  the tab is connected to.

## [1.3.7] - 2026-06-30

### Added

- **Back up and restore SQL Server databases from the schema tree.**
  Right-click a database for **Back Up…** (`BACKUP DATABASE` to a `.bak`) or
  **Restore…** (lay a `.bak` over the existing database, replacing its contents
  while keeping its name — the backup may come from a different database).
  Restore previews the backup's contents and requires typing the database name
  to confirm. Both show live progress (a real percentage when the server
  reports it) and can be cancelled.
- **Browse the SQL Server's own folders to pick a backup location.** Because
  backups are written and read on the **server** (not your machine), the dialogs
  use a server-side path with a **Browse…** button that walks the server's
  filesystem, pre-filled with the server's default backup directory. For a local
  server whose backup folder is shared to your machine, the file then appears
  locally.
- **Backup defaults in Settings.** Set whether new backups use compression,
  checksums, and verify-after-backup under **Settings → General → Database
  backup**.
- **Optionally delete the backup file after a successful restore.** A checkbox
  in the Restore dialog removes the `.bak` from the server afterwards
  (best-effort; requires `xp_cmdshell` enabled — if it isn't, the restore still
  succeeds and you're told the file was left in place).

## [1.3.6] - 2026-06-29

### Added

- **Re-import a CSV as a new table after a failed attempt, without leaving the
  app.** When importing a CSV as a new table fails after the table was already
  created, retrying no longer dead-ends on _"There is already an object
  named…"_. The import dialog now offers **Drop table & retry** — a two-step
  confirm that names the table before it drops it — then re-runs the import.

### Fixed

- **CSV files that aren't UTF-8 now import correctly.** Files exported from
  Excel on Windows (ISO-8859-1 / Windows-1252) used to fail the moment a
  special character like "é" appeared. Selene now detects the file's encoding
  and transcodes it to UTF-8 automatically — for both the preview/mapping step
  and the import itself.

## [1.3.5] - 2026-06-29

### Added

- **Copy query results in the format you need — and paste cleanly into Excel.**
  Copying selected cells (or a whole result set) now produces **tab-separated**
  text by default, so it pastes into proper spreadsheet columns instead of
  landing in a single comma-joined cell. Choose the default format — Tab, Comma
  (CSV), Markdown, or HTML — under **Settings → Results → "Copy format"**, or
  right-click the grid and pick **Copy as ›** for a one-off. A new **"Include
  headers when copying"** toggle prepends the column-name row (HTML copies paste
  as a real table into Excel/Word).
- **Select the whole result set with Cmd/Ctrl+A.** Press it with the grid
  focused to select every cell, then copy in one go.

## [1.3.4] - 2026-06-26

### Changed

- **Settings are tidier: CSV options now share one tab, and "Backup" is renamed
  to "Backup & restore".** The former separate Export and Import tabs — both only
  CSV format options — are now Export and Import sections under a single **CSV**
  tab. The Backup tab keeps the same connection-and-settings backup/restore
  actions but is renamed so it no longer reads like a CSV export/import.

### Removed

- **The separate delimiter and UTF-8 BOM for a multi-target run's combined CSV.**
  Saving the aggregated results of a "Run on multiple targets" now uses the same
  delimiter and BOM as the **CSV → Export** settings, so CSV format lives in one
  place. The defaults were identical, so existing setups are unaffected.

## [1.3.3] - 2026-06-26

### Added

- **Run on multiple targets now pauses and asks when too many targets fail.**
  Once the share of failed targets reaches a threshold, the run stops starting
  new databases and a dialog offers **Continue** (run the rest) or **Stop**. This
  catches a query that's failing everywhere early, and quiets the progress
  updates that made a big run feel sluggish and the Stop button hard to hit. The
  threshold is a percentage of the run's planned targets — set it (or turn it
  off) under **Settings → Multi-target → "Pause after failures reach"**, default
  10%. The run prompts at most once; after Continue it runs to the end.

## [1.3.2] - 2026-06-26

### Added

- **Windows gets an in-app settings button and integrated window controls.**
  Since Windows has no native menu bar, the title bar now carries a settings
  gear (also opened with `Ctrl+,`) plus its own minimize / maximize / close
  buttons, and the native OS title bar is hidden so the app owns the whole top
  strip. macOS is unchanged — it keeps its native menu and title bar.

### Changed

- **The right-click menu no longer shows the browser's defaults** (Save as,
  Print, Share). The native context menu is suppressed everywhere except inside
  the SQL editor and text fields, where copy/paste stays available.
- **The Windows installer now ships as a single `.exe`** (the NSIS setup),
  dropping the separate `.msi`.

## [1.3.1] - 2026-06-26

### Changed

- **Run on multiple targets now previews the matched databases in a modal, and
  gates the run on it.** In filter-query mode the action row shows a single
  **Preview databases** button; clicking it lists exactly which databases match
  (per server) in a dialog. Only after previewing do **Generate script**,
  **Execute**, and **Fetch results** appear — and editing the filter query hides
  them again until you re-preview, so a run can never target a stale or unseen
  database set. Picking databases by hand from a list is unchanged.

## [1.3.0] - 2026-06-25

### Added

- **Run on multiple targets.** A new main-area view (open it from the
  Connections panel header) runs one SQL batch across many databases on many
  saved servers. Pick the servers, then choose the databases per server either
  by a filter query — with a **Preview** that lists exactly which databases
  match — or by hand from a list. Then:
  - **Generate script** — produce a copy-pasteable `USE [db] … RAISERROR … PRINT`
    script per server, opened in new editor tabs.
  - **Execute** — run the batch on every selected database, with live per-target
    success/failure progress and a **Stop** button. Each database shows its
    rows-affected count, and any whose DML changed **0 rows** is highlighted.
    Filter the progress list by outcome (OK / 0-affected / Failed) to isolate
    just the problems.
  - **Fetch results** — run a SELECT on every database and aggregate the rows
    (each prefixed with `_server` / `_database`) into the results grid, then
    **Save CSV**.

  Servers run in parallel (configurable), databases sequentially per server. The
  SQL guard and each connection's read-only flag are enforced. New **Settings →
  Multi-target** options: the default database-filter query, max parallel
  servers, and the combined-CSV delimiter / BOM.

## [1.2.3] - 2026-06-22

### Added

- **Reorder open editor tabs by dragging them.** Drag a query tab along the tab
  strip to drop it into a new position; an accent insertion line shows where it
  will land.

## [1.2.2] - 2026-06-22

### Fixed

- **Cmd/Ctrl+Enter on the SQL guard prompt now reliably runs the query.** A
  second press while the confirm prompt was open could start a parallel run and
  re-open the prompt instead of confirming; the run is now guarded against
  re-entry.
- **The editor no longer scroll-jumps to the top when you click into it after
  running a query.** Focus is returned to the editor after a Run-button click and
  after the guard prompt closes, so clicking back in (e.g. double-clicking to
  select) no longer scrolls you away.

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
