/**
 * Hand-written TypeScript mirrors of the Rust IPC contract.
 *
 * CASING CONTRACT (verified against `src-tauri/src/commands/*.rs` and
 * `crates/selene-core/src/*.rs`):
 *  - IPC *event envelopes* (QueryEvent / ExportEvent) and the SessionInfo
 *    wrapper use **camelCase** (`#[serde(rename_all = "camelCase")]`):
 *    `queryId`, `setIndex`, `elapsedMs`, `sessionId`.
 *  - The embedded **core domain types keep snake_case** field names:
 *    `Column { db_type, logical }`, `ConnectionSpec { read_only, ... }`,
 *    `ColumnInfo { data_type, is_primary_key, max_length }`, etc.
 *  - `CellValue` is adjacently tagged `{ "t": <variant>, "v": <payload> }`
 *    (serde `tag = "t", content = "v"`); `Null` and `Cancelled` carry no `v`.
 *  - Enums like LogicalType / TemporalKind / TableKind / DriverId / ExportFormat
 *    are lowercase or snake_case scalar strings (see each `rename_all`).
 *
 * These types are not auto-generated (ts-rs is deferred); keep them in lockstep
 * with the Rust structs by hand.
 */

// ---------------------------------------------------------------------------
// Core value types (snake_case fields; CellValue is `{t, v}` tagged)
// ---------------------------------------------------------------------------

/** Which flavour of temporal value a {@link CellValue} of kind `DateTime` holds. */
export type TemporalKind = "date" | "time" | "date_time" | "date_time_offset";

/**
 * A single result-set cell, adjacently tagged on the wire as `{ t, v }`.
 * `Decimal` is a string to preserve exact numeric precision; `Bytes` is a byte
 * array; unmodelled server types arrive as `Unsupported` (lossless text).
 */
export type CellValue =
  | { t: "Null" }
  | { t: "Bool"; v: boolean }
  | { t: "I64"; v: number }
  | { t: "F64"; v: number }
  | { t: "Decimal"; v: string }
  | { t: "String"; v: string }
  | { t: "Bytes"; v: number[] }
  | { t: "DateTime"; v: { iso: string; kind: TemporalKind } }
  | { t: "Uuid"; v: string }
  | { t: "Unsupported"; v: { type_name: string; text: string } };

/** Coarse, driver-neutral bucket for alignment/formatting (snake_case scalar). */
export type LogicalType =
  | "null"
  | "boolean"
  | "integer"
  | "float"
  | "decimal"
  | "text"
  | "binary"
  | "date"
  | "time"
  | "date_time"
  | "uuid"
  | "json"
  | "other";

/** Metadata for one result-set column (snake_case fields). */
export interface Column {
  name: string;
  ordinal: number;
  db_type: string;
  logical: LogicalType;
  /** `null` when nullability is unknown. */
  nullable: boolean | null;
}

/** Summary of a completed execution (snake_case fields). */
export interface ExecOutcome {
  result_sets: number;
  total_rows: number;
  truncated: boolean;
  /** True for a rollback-wrapped dry-run (`BEGIN TRAN; <DML …>; ROLLBACK`):
   * the affected counts are what *would* have changed; nothing was committed. */
  rolled_back: boolean;
}

// ---------------------------------------------------------------------------
// Streaming events (camelCase envelopes, `kind`-tagged)
// ---------------------------------------------------------------------------

/**
 * Events streamed by `query_run` over a `Channel<QueryEvent>`. Lifecycle:
 * `started` -> (`meta`, `rows`*, `setEnd`)+ -> `finished`, or a terminal
 * `cancelled` / `failed`.
 */
export type QueryEvent =
  | { kind: "started"; queryId: string }
  | { kind: "meta"; setIndex: number; columns: Column[] }
  | { kind: "rows"; setIndex: number; rows: CellValue[][] }
  | { kind: "setEnd"; setIndex: number; affected: number | null }
  | { kind: "finished"; outcome: ExecOutcome; elapsedMs: number }
  | { kind: "cancelled" }
  | { kind: "failed"; message: string };

/** Progress events streamed by `export_result` over a `Channel<ExportEvent>`. */
export type ExportEvent =
  | { kind: "progress"; rows: number }
  | { kind: "done"; rows: number }
  | { kind: "failed"; message: string };

/** Target on-disk format for an export (lowercase scalar). */
export type ExportFormat = "csv" | "json" | "xlsx";

/**
 * CSV-specific export options passed to `export_result` (snake_case fields to
 * match the Rust `CsvExportOptions` struct). Ignored for JSON/XLSX.
 */
export interface CsvOptions {
  delimiter: string;
  quote: string;
  quote_style: string;
  line_ending: string;
  include_header: boolean;
  bom: boolean;
}

/** Outcome of a completed export (snake_case field). */
export interface ExportSummary {
  rows_written: number;
}

// ---------------------------------------------------------------------------
// Import (CSV → table). Event envelope + analysis use camelCase; the embedded
// ImportSummary / InferredType keep snake_case, matching their serde derives.
// ---------------------------------------------------------------------------

/** Progress events streamed by `import_csv` over a `Channel<ImportEvent>`. */
export type ImportEvent =
  | { kind: "progress"; rows: number }
  | { kind: "done"; inserted: number; skipped: number }
  | { kind: "failed"; message: string };

/** Outcome of a completed import (snake_case fields). */
export interface ImportSummary {
  rows_inserted: number;
  rows_skipped: number;
}

/** A SQL Server type inferred for one CSV column (snake_case `sql_type`). */
export interface InferredType {
  sql_type: string;
  logical: LogicalType;
}

/** Result of `import_csv_analyze`: header, a sample, and per-column inference. */
export interface CsvAnalysis {
  headers: string[];
  sampleRows: string[][];
  inferred: InferredType[];
  /** First few raw, unparsed lines — shows the delimiter regardless of options. */
  rawPreview: string[];
}

/**
 * CSV parse/import options sent to the import commands. camelCase keys to match
 * the Rust `CsvImportOptionsArg` (`#[serde(rename_all = "camelCase")]`); every
 * field is optional and falls back to a sensible default backend-side.
 */
export interface ImportCsvOptions {
  delimiter?: string;
  quote?: string;
  hasHeader?: boolean;
  emptyAsNull?: boolean;
  atomic?: boolean;
}

/** One column of a new table to create during import (camelCase). */
export interface NewColumnDef {
  name: string;
  sqlType: string;
  nullable: boolean;
}

/** The import destination: an existing table, or a new table to create. */
export type ImportTargetArg =
  | { kind: "existing"; database: string | null; schema: string; table: string }
  | {
      kind: "new";
      database: string | null;
      schema: string;
      table: string;
      columns: NewColumnDef[];
    };

/** Maps one destination column to its CSV source field (`null` = insert NULL). */
export interface ColumnMappingArg {
  csvIndex: number | null;
  targetColumn: string;
}

// ---------------------------------------------------------------------------
// Database backup & restore (.bak). Event envelopes are camelCase + `kind`-
// tagged; BackupFile is a core domain type and keeps snake_case. Mirrors
// `src-tauri/src/commands/backup.rs` + the BackupEvent/RestoreEvent enums in
// `commands/mod.rs`.
// ---------------------------------------------------------------------------

/**
 * Progress events streamed by `database_backup` over a `Channel<BackupEvent>`.
 * Lifecycle: `started` → `progress`* → (`done` | `cancelled` | `failed`).
 * `progress` may never arrive (e.g. the polling connection lacks
 * `VIEW SERVER STATE`), in which case the UI shows indeterminate progress.
 */
export type BackupEvent =
  | { kind: "started"; operationId: string }
  | { kind: "progress"; percent: number }
  | { kind: "done" }
  | { kind: "cancelled" }
  | { kind: "failed"; message: string };

/** Progress events streamed by `database_restore`. Same shape as BackupEvent. */
export type RestoreEvent =
  | { kind: "started"; operationId: string }
  | { kind: "progress"; percent: number }
  | { kind: "done" }
  | { kind: "cancelled" }
  | { kind: "failed"; message: string };

/** One logical file inside a `.bak`, from `RESTORE FILELISTONLY` (snake_case
 *  core type). `file_type`: "D" data, "L" log, "F" full-text, "S" filestream. */
export interface BackupFile {
  logical_name: string;
  physical_name: string;
  file_type: string;
}

/** Backup options sent to `database_backup` (camelCase). */
export interface BackupOptionsArg {
  compression: boolean;
  checksum: boolean;
  verifyAfter: boolean;
}

/** Restore options sent to `database_restore` (camelCase). */
export interface RestoreOptionsArg {
  checksum: boolean;
}

/** Resolved result of an awaited backup/restore command (camelCase). The
 *  channel events carry the live status; this just unblocks the promise. */
export interface OperationSummary {
  elapsedMs: number;
  cancelled: boolean;
}

// ---------------------------------------------------------------------------
// Run on multiple targets (camelCase envelopes; embedded Column/CellValue keep
// their own casing). Mirrors `src-tauri/src/commands/multi.rs` + the MultiEvent
// enum in `commands/mod.rs`.
// ---------------------------------------------------------------------------

/** What `multi_target_run` does on each target (lowercase scalar). */
export type MultiMode = "execute" | "results";

/** One run target: a saved connection plus the explicit databases to run on. */
export interface MultiTarget {
  connectionId: string;
  databases: string[];
}

/** One entry of `multi_target_resolve`: the databases a filter query matched on
 *  a connection, or the (sanitized) error it produced. `error` is omitted on
 *  success (`skip_serializing_if`). */
export interface ResolvedTarget {
  connectionId: string;
  /** Connection display name; used as the `_server` label. */
  server: string;
  databases: string[];
  error?: string;
}

/** Returned by `multi_target_run`: the id used to cancel the run. */
export interface MultiRunHandle {
  runId: string;
}

/**
 * Events streamed by `multi_target_run` over a `Channel<MultiEvent>`.
 * `execute`: `started` → (`target`, `targetDone`)* / `serverError`* →
 * `finished`. `results` additionally emits one `meta` (unified columns, with
 * `_server`/`_database` prepended) and `rows` batches. Can end in `cancelled`.
 * If the failure rate crosses the configured threshold, a single `paused`
 * arrives and the run idles until resumed (more events) or cancelled.
 */
export type MultiEvent =
  | { kind: "started"; runId: string; total: number }
  | {
      kind: "target";
      connectionId: string;
      server: string;
      database: string;
      index: number;
      total: number;
    }
  | { kind: "meta"; columns: Column[] }
  | { kind: "rows"; rows: CellValue[][] }
  | {
      kind: "targetDone";
      connectionId: string;
      server: string;
      database: string;
      index: number;
      rows: number | null;
      error: string | null;
    }
  | { kind: "serverError"; connectionId: string; server: string; error: string }
  | { kind: "paused"; failed: number; total: number }
  | { kind: "finished"; succeeded: number; failed: number; rowsTotal: number }
  | { kind: "cancelled" };

// ---------------------------------------------------------------------------
// Connection / session types
// ---------------------------------------------------------------------------

/** Backend driver id (lowercase scalar). Only `mssql` is live in v0.1. */
export type DriverId = "mssql";

/**
 * How to authenticate. Tagged by `method` (snake_case). Only `sql_login` exists
 * today; the Rust enum is `#[non_exhaustive]` for future Windows/Entra auth.
 */
export interface AuthMethod {
  method: "sql_login";
  username: string;
}

/** Transport security settings (snake_case fields). */
export interface TlsConfig {
  encrypt: boolean;
  trust_server_certificate: boolean;
}

/** A saved connection's non-secret configuration (snake_case fields). */
export interface ConnectionSpec {
  id: string;
  name: string;
  driver: DriverId;
  host: string;
  port: number | null;
  instance: string | null;
  database: string | null;
  auth: AuthMethod;
  tls: TlsConfig;
  read_only: boolean;
}

/** Per-driver capability flags (snake_case fields). Gates UI features. */
export interface DriverCapabilities {
  schemas: boolean;
  multiple_result_sets: boolean;
  server_side_cancel: boolean;
  transactions: boolean;
  explain_plan: boolean;
  streaming_rows: boolean;
  list_databases: boolean;
  data_editing: boolean;
}

/** Returned by `session_connect` (camelCase envelope). */
export interface SessionInfo {
  sessionId: string;
  driver: DriverId;
  capabilities: DriverCapabilities;
}

/**
 * Payload of the global `"session:lost"` Tauri event, emitted by the backend
 * health-check heartbeat when a live session's connection has dropped and the
 * session was auto-closed. The frontend uses it to remove the session and offer
 * a reconnect for the originating connection.
 */
export interface SessionLostEvent {
  sessionId: string;
  connectionId: string;
}

// ---------------------------------------------------------------------------
// Introspection types (snake_case fields)
// ---------------------------------------------------------------------------

export interface DatabaseInfo {
  name: string;
  is_system: boolean;
  /** Availability state, e.g. "ONLINE", "OFFLINE". */
  state_desc: string;
}

export interface SchemaInfo {
  name: string;
}

export type TableKind = "table" | "view";

export interface TableInfo {
  schema: string;
  name: string;
  kind: TableKind;
}

export interface ColumnInfo {
  name: string;
  ordinal: number;
  data_type: string;
  nullable: boolean;
  is_primary_key: boolean;
  max_length: number | null;
}

// ---------------------------------------------------------------------------
// Filesystem (file-backed tabs + workspace folders) — camelCase fields
// ---------------------------------------------------------------------------

/** One child of a listed directory: a subdirectory or a `.sql` file. */
export interface FsEntry {
  /** Final path component (file or directory name). */
  name: string;
  /** Canonical absolute path. */
  path: string;
  isDir: boolean;
}

/**
 * A change to a watched file, delivered on the global `"fs:change"` Tauri event
 * (not a per-call Channel — the watcher outlives any one command). `path` is in
 * the same canonical form stored on tabs, so it matches directly.
 */
export type FsEvent =
  | { kind: "changed"; path: string }
  | { kind: "removed"; path: string };

// ---------------------------------------------------------------------------
// Guard / test / error
// ---------------------------------------------------------------------------

export type GuardLevel = "info" | "confirm" | "block";

export interface GuardVerdict {
  level: GuardLevel;
  reasons: string[];
}

export interface TestReport {
  server_version: string | null;
  elapsed_ms: number;
}

/**
 * The shape `invoke` rejects with — every command returns
 * `Result<T, IpcError>` and the JS promise rejects with this object on `Err`.
 * `message` is sanitized (secret-free); `kind` is a stable discriminant.
 */
export interface IpcError {
  message: string;
  kind: string;
}

/** Narrow an unknown thrown value to an {@link IpcError} where possible. */
export function asIpcError(err: unknown): IpcError {
  if (
    typeof err === "object" &&
    err !== null &&
    "message" in err &&
    typeof (err as { message: unknown }).message === "string"
  ) {
    const e = err as { message: string; kind?: unknown };
    return {
      message: e.message,
      kind: typeof e.kind === "string" ? e.kind : "unknown",
    };
  }
  if (typeof err === "string") return { message: err, kind: "unknown" };
  return { message: "An unexpected error occurred.", kind: "unknown" };
}
