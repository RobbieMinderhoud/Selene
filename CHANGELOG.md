# Changelog

All notable **functional** (user-facing) changes to Selene are documented here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Internal refactors, tooling, and chores are intentionally omitted. (Automated
changelog generation from Conventional Commits via git-cliff will be added once
the project is under version control.)

## [Unreleased]

v0.1 is under active development. The `selene-core` data layer, the Tauri IPC
layer, and the desktop UI are all implemented; remaining work is broader test
coverage (dockerized-MSSQL integration tests, frontend component/E2E tests) and
packaging.

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
