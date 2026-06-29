/**
 * Typed wrappers over the Tauri `invoke` surface.
 *
 * Tauri v2 maps **camelCase JS argument keys** to the snake_case Rust command
 * parameters automatically (e.g. JS `{ sessionId }` -> Rust `session_id`,
 * `{ connectionId }` -> `connection_id`, `{ readOnly }` -> `read_only`,
 * `{ maxRows }` -> `max_rows`, `{ onEvent }` -> `on_event`). So every arg object
 * below uses camelCase keys. This was verified against the command signatures
 * in `src-tauri/src/commands/*.rs`.
 *
 * `invoke` rejects with the `IpcError` shape on `Err`; callers should catch and
 * surface via `asIpcError`.
 */

import { Channel, invoke } from "@tauri-apps/api/core";

import type {
  CellValue,
  Column,
  ColumnInfo,
  ColumnMappingArg,
  ConnectionSpec,
  CsvAnalysis,
  CsvOptions,
  DatabaseInfo,
  ExportEvent,
  ExportFormat,
  ExportSummary,
  FsEntry,
  GuardVerdict,
  ImportCsvOptions,
  ImportEvent,
  ImportSummary,
  ImportTargetArg,
  MultiEvent,
  MultiMode,
  MultiRunHandle,
  MultiTarget,
  QueryEvent,
  ResolvedTarget,
  SchemaInfo,
  SessionInfo,
  TableInfo,
  TestReport,
} from "./types";

// --- Connections ----------------------------------------------------------

/** `connections_list` -> all saved connection specs (non-secret). */
export function connectionsList(): Promise<ConnectionSpec[]> {
  return invoke("connections_list");
}

/**
 * `connection_save` -> upsert a spec. When `password` is provided it is written
 * to the OS keychain; when omitted the stored secret is left untouched. The
 * password is passed straight through and never stored in app state.
 */
export function connectionSave(
  spec: ConnectionSpec,
  password?: string,
): Promise<ConnectionSpec> {
  return invoke("connection_save", { spec, password });
}

/** `connection_delete` -> remove a spec and its keychain secret. */
export function connectionDelete(id: string): Promise<void> {
  return invoke("connection_delete", { id });
}

/** `connection_reorder` -> persist a new display order for saved connections. */
export function connectionReorder(ids: string[]): Promise<void> {
  return invoke("connection_reorder", { ids });
}

/**
 * `connections_import` -> upsert a list of specs by id (merge, not replace).
 * Returns the full updated list. Passwords are not imported — the user is
 * prompted when they first connect each imported connection.
 */
export function connectionsImport(
  specs: ConnectionSpec[],
): Promise<ConnectionSpec[]> {
  return invoke("connections_import", { specs });
}

/** `connection_test` -> probe connectivity without opening a session. */
export function connectionTest(
  spec: ConnectionSpec,
  password?: string,
): Promise<TestReport> {
  return invoke("connection_test", { spec, password });
}

// --- Sessions -------------------------------------------------------------

/**
 * `session_connect` -> open a live session for a saved connection.
 *
 * Pass `password` to authenticate with a user-supplied password (the retry
 * after the missing-password prompt); on success the backend persists it to the
 * keychain. Omit it to use the stored secret — a missing one rejects with
 * `kind: "secret"`, which {@link connectSession} catches to drive the prompt.
 */
export function sessionConnect(
  connectionId: string,
  password?: string,
): Promise<SessionInfo> {
  return invoke("session_connect", { connectionId, password });
}

/** `session_disconnect` -> close a live session (idempotent). */
export function sessionDisconnect(sessionId: string): Promise<void> {
  return invoke("session_disconnect", { sessionId });
}

/** `session_use_database` -> switch the active database for the session. */
export function sessionUseDatabase(
  sessionId: string,
  database: string,
): Promise<void> {
  return invoke("session_use_database", { sessionId, database });
}

/** `session_current_database` -> the active database name for the session. */
export function sessionCurrentDatabase(sessionId: string): Promise<string> {
  return invoke("session_current_database", { sessionId });
}

/** `session_create_database` -> create a new database. Refused on read-only. */
export function sessionCreateDatabase(
  sessionId: string,
  database: string,
): Promise<void> {
  return invoke("session_create_database", { sessionId, database });
}

/**
 * `session_drop_database` -> drop a database. Refused on read-only; fails if the
 * database is in use by other connections.
 */
export function sessionDropDatabase(
  sessionId: string,
  database: string,
): Promise<void> {
  return invoke("session_drop_database", { sessionId, database });
}

/**
 * `session_rename_database` -> rename a database. Refused on read-only.
 *
 * With `force = false` it rejects with `kind === "database_in_use"` when the
 * database has active connections (instead of hanging); retry with
 * `force = true` to disconnect those sessions and complete the rename.
 */
export function sessionRenameDatabase(
  sessionId: string,
  from: string,
  to: string,
  force: boolean,
): Promise<void> {
  return invoke("session_rename_database", { sessionId, from, to, force });
}

/**
 * `session_set_database_online` -> bring a database online (`online = true`)
 * or take it offline (`online = false`, terminating all connections to it).
 * Refused on a read-only connection.
 */
export function sessionSetDatabaseOnline(
  sessionId: string,
  database: string,
  online: boolean,
): Promise<void> {
  return invoke("session_set_database_online", { sessionId, database, online });
}

// --- Connection health ----------------------------------------------------

/**
 * `set_health_check` -> tune the backend heartbeat that pings live sessions and
 * auto-closes dropped ones. Call on startup and whenever the user changes the
 * health settings. `intervalSecs` is clamped to a floor server-side.
 */
export function setHealthCheck(
  enabled: boolean,
  intervalSecs: number,
): Promise<void> {
  return invoke("set_health_check", { enabled, intervalSecs });
}

// --- Introspection --------------------------------------------------------

/** `databases_list` -> databases on the session's server. */
export function databasesList(sessionId: string): Promise<DatabaseInfo[]> {
  return invoke("databases_list", { sessionId });
}

/** `schemas_list` -> schemas within `database`. */
export function schemasList(
  sessionId: string,
  database: string,
): Promise<SchemaInfo[]> {
  return invoke("schemas_list", { sessionId, database });
}

/** `tables_list` -> tables and views within `database`.`schema`. */
export function tablesList(
  sessionId: string,
  database: string,
  schema: string,
): Promise<TableInfo[]> {
  return invoke("tables_list", { sessionId, database, schema });
}

/** `columns_list` -> columns of `database`.`schema`.`table`. */
export function columnsList(
  sessionId: string,
  database: string,
  schema: string,
  table: string,
): Promise<ColumnInfo[]> {
  return invoke("columns_list", { sessionId, database, schema, table });
}

// --- Guard ----------------------------------------------------------------

/** `guard_check` -> classify a SQL batch for safety before running it. */
export function guardCheck(
  sql: string,
  readOnly: boolean,
): Promise<GuardVerdict> {
  return invoke("guard_check", { sql, readOnly });
}

// --- Query (streaming) ----------------------------------------------------

/**
 * `query_run` -> start a streaming query. Result data arrives on `onEvent`
 * (a `Channel<QueryEvent>`); the returned `{ queryId }` is the cancellation
 * handle. The command returns immediately, before the first row.
 */
export function queryRun(
  sessionId: string,
  sql: string,
  maxRows: number | undefined,
  onEvent: Channel<QueryEvent>,
): Promise<{ queryId: string }> {
  return invoke("query_run", { sessionId, sql, maxRows, onEvent });
}

/** `query_cancel` -> request cooperative cancellation of an in-flight query. */
export function queryCancel(queryId: string): Promise<void> {
  return invoke("query_cancel", { queryId });
}

// --- Export (streaming) ---------------------------------------------------

/**
 * `export_result` -> run a query and write its first result set to `path`.
 * Progress arrives on `onProgress` (a `Channel<ExportEvent>`); the awaited
 * result is the final `ExportSummary`.
 */
export function exportResult(
  sessionId: string,
  sql: string,
  format: ExportFormat,
  path: string,
  maxRows: number | undefined,
  onProgress: Channel<ExportEvent>,
  csvOptions?: CsvOptions,
): Promise<ExportSummary> {
  return invoke("export_result", {
    sessionId,
    sql,
    format,
    path,
    maxRows,
    csvOptions,
    onProgress,
  });
}

// --- Run on multiple targets ----------------------------------------------

/**
 * `multi_target_resolve` -> run `filterSql` against each connection's current
 * server and return the database names it yields (column 0), or a per-connection
 * error. Powers the run preview, the target count, and the "generate script"
 * plan. Sequential server-by-server (it is a preview, not the hot path).
 */
export function multiTargetResolve(
  connectionIds: string[],
  filterSql: string,
): Promise<ResolvedTarget[]> {
  return invoke("multi_target_resolve", { connectionIds, filterSql });
}

/**
 * `multi_target_run` -> run `sql` across every (server, database) in `targets`.
 * Progress (and, in `results` mode, aggregated rows prefixed with
 * `_server`/`_database`) arrive on `onEvent` (a `Channel<MultiEvent>`); the
 * returned `{ runId }` is the cancellation handle. Returns immediately.
 */
export function multiTargetRun(
  targets: MultiTarget[],
  sql: string,
  mode: MultiMode,
  maxRows: number | undefined,
  maxParallel: number | undefined,
  pauseFailurePercent: number | undefined,
  onEvent: Channel<MultiEvent>,
): Promise<MultiRunHandle> {
  return invoke("multi_target_run", {
    targets,
    sql,
    mode,
    maxRows,
    maxParallel,
    pauseFailurePercent,
    onEvent,
  });
}

/** `multi_target_cancel` -> request cooperative cancellation of a multi run. */
export function multiTargetCancel(runId: string): Promise<void> {
  return invoke("multi_target_cancel", { runId });
}

/**
 * `multi_target_resume` -> continue a run that auto-paused after its failure
 * rate crossed the threshold (the user chose "Continue" in the prompt).
 */
export function multiTargetResume(runId: string): Promise<void> {
  return invoke("multi_target_resume", { runId });
}

/**
 * `export_result_set` -> write an already-collected result set (`columns` +
 * `rows`) to `path` via the core exporter. Backs "Save CSV" for the aggregated
 * multi-target grid without re-running the queries.
 */
export function exportResultSet(
  columns: Column[],
  rows: CellValue[][],
  format: ExportFormat,
  path: string,
  csvOptions?: CsvOptions,
): Promise<ExportSummary> {
  return invoke("export_result_set", {
    columns,
    rows,
    format,
    path,
    csvOptions,
  });
}

// --- Import (CSV) ---------------------------------------------------------

/**
 * `import_csv_analyze` -> read a CSV's header + a sample and infer a SQL type
 * per column, so the mapping menu can render before anything is written. Needs
 * no session (pure file read).
 */
export function importCsvAnalyze(
  path: string,
  options?: ImportCsvOptions,
): Promise<CsvAnalysis> {
  return invoke("import_csv_analyze", { path, options });
}

/**
 * `import_csv` -> import a CSV into an existing or new table. Progress arrives
 * on `onProgress` (a `Channel<ImportEvent>`); the awaited result is the final
 * `ImportSummary`. `mapping` describes each destination column's CSV source.
 */
export function importCsv(
  sessionId: string,
  path: string,
  target: ImportTargetArg,
  mapping: ColumnMappingArg[],
  options: ImportCsvOptions,
  onProgress: Channel<ImportEvent>,
): Promise<ImportSummary> {
  return invoke("import_csv", {
    sessionId,
    path,
    target,
    mapping,
    options,
    onProgress,
  });
}

/**
 * `table_drop` -> drop a table. Backs the import modal's "replace existing"
 * recovery (drop then retry an *import as new table* that failed because the
 * table already existed). Refused on a read-only connection.
 */
export function tableDrop(
  sessionId: string,
  database: string | null,
  schema: string,
  table: string,
): Promise<void> {
  return invoke("table_drop", { sessionId, database, schema, table });
}

// --- Filesystem (file-backed tabs + workspace folders) --------------------

/** `file_read` -> a text file's contents (UTF-8). */
export function fileRead(path: string): Promise<string> {
  return invoke("file_read", { path });
}

/** `file_write` -> write `content` to `path` atomically + byte-faithfully. */
export function fileWrite(path: string, content: string): Promise<void> {
  return invoke("file_write", { path, content });
}

/** `dir_list` -> immediate subdirectories + `.sql` files (dirs first). */
export function dirList(path: string): Promise<FsEntry[]> {
  return invoke("dir_list", { path });
}

/** `canonicalize_path` -> the canonical absolute form of a (dialog) path. */
export function canonicalizePath(path: string): Promise<string> {
  return invoke("canonicalize_path", { path });
}

/** `fs_watch` -> recursively watch a folder for `.sql` changes (idempotent). */
export function fsWatch(path: string): Promise<void> {
  return invoke("fs_watch", { path });
}

/** `fs_unwatch` -> stop watching a folder (idempotent). */
export function fsUnwatch(path: string): Promise<void> {
  return invoke("fs_unwatch", { path });
}
