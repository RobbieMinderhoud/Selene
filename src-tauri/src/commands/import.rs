//! CSV import commands.
//!
//! Two commands back the import flow:
//! - [`import_csv_analyze`] reads a CSV's header + a sample and infers a SQL
//!   type per column, so the mapping menu can render before anything is written.
//!   It needs no session (pure file read).
//! - [`import_csv`] performs the import: it (optionally) creates the target
//!   table, then streams the CSV through a [`CsvRowSource`] into the driver's
//!   bound-parameter insert path, forwarding progress over a `Channel`.
//!
//! Like [`export_result`](super::export::export_result), `import_csv` **awaits**
//! to completion and returns an [`ImportSummary`]; the frontend drives it with a
//! progress channel and a single awaited result.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use tauri::ipc::Channel;
use tauri::State;

use selene_core::{
    analyze_csv, logical_for_sql_type, CancelToken, CellValue, Connection, CoreError, CsvAnalysis,
    CsvImportOptions, CsvRowSource, DestColumn, ImportSummary, ImportTarget, NewColumn, RowSource,
};

use crate::commands::ImportEvent;
use crate::error::IpcError;
use crate::state::AppState;

/// Rows pulled per source batch (also the desired rows-per-INSERT; the driver
/// sub-batches further to respect SQL Server's parameter limit).
const BATCH_SIZE: usize = 500;

/// How many data rows to sample when inferring column types for the menu.
const ANALYZE_SAMPLE: usize = 1000;

/// CSV import options from the frontend (camelCase, all optional → defaults).
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CsvImportOptionsArg {
    delimiter: Option<String>,
    quote: Option<String>,
    has_header: Option<bool>,
    empty_as_null: Option<bool>,
    atomic: Option<bool>,
}

impl CsvImportOptionsArg {
    fn into_core(self) -> CsvImportOptions {
        // A quote of "none" (or empty) disables quote handling entirely; any
        // other value contributes its first byte as the quote character.
        let (quote, quoting) = match self.quote.as_deref() {
            Some("none") | Some("") => (b'"', false),
            Some(s) => (s.as_bytes().first().copied().unwrap_or(b'"'), true),
            None => (b'"', true),
        };
        CsvImportOptions {
            delimiter: first_byte(self.delimiter.as_deref(), b','),
            quote,
            quoting,
            has_header: self.has_header.unwrap_or(true),
            empty_as_null: self.empty_as_null.unwrap_or(true),
            atomic: self.atomic.unwrap_or(true),
        }
    }
}

fn first_byte(s: Option<&str>, fallback: u8) -> u8 {
    s.and_then(|v| v.as_bytes().first().copied())
        .unwrap_or(fallback)
}

/// One destination column of a new table to be created during import.
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewColumnArg {
    name: String,
    sql_type: String,
    nullable: bool,
}

/// The import destination: an existing table, or a new table to create.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum ImportTargetArg {
    /// Insert into an existing table; columns are resolved by introspection.
    #[serde(rename_all = "camelCase")]
    Existing {
        database: Option<String>,
        schema: String,
        table: String,
    },
    /// Create `table` from `columns`, then insert.
    #[serde(rename_all = "camelCase")]
    New {
        database: Option<String>,
        schema: String,
        table: String,
        columns: Vec<NewColumnArg>,
    },
}

/// One destination column's source: which CSV field feeds it (or `null` for an
/// explicit `NULL`), keyed by the destination column name.
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ColumnMappingArg {
    csv_index: Option<usize>,
    target_column: String,
}

/// Read a CSV's header + a sample and infer a type per column for the menu.
#[tauri::command]
pub async fn import_csv_analyze(
    path: String,
    options: Option<CsvImportOptionsArg>,
) -> Result<CsvAnalysis, IpcError> {
    let opts = options.unwrap_or_default().into_core();
    let file = PathBuf::from(&path);
    // Sampling a large file is blocking I/O; keep it off the async runtime.
    let analysis = tokio::task::spawn_blocking(move || analyze_csv(&file, &opts, ANALYZE_SAMPLE))
        .await
        .map_err(|e| IpcError::new("io", format!("analyze task failed: {e}")))??;
    Ok(analysis)
}

/// Import a CSV into the target table (existing or new), streaming progress.
// A Tauri command surface: each argument maps to a JS IPC field.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn import_csv(
    state: State<'_, AppState>,
    session_id: String,
    path: String,
    target: ImportTargetArg,
    mapping: Vec<ColumnMappingArg>,
    options: Option<CsvImportOptionsArg>,
    on_progress: Channel<ImportEvent>,
) -> Result<ImportSummary, IpcError> {
    let core_opts = options.unwrap_or_default().into_core();
    let file = PathBuf::from(&path);
    let cancel = CancelToken::new();

    // Always emit a terminal event before returning an error, so the channel has
    // a known lifecycle on the frontend.
    let fail = |ch: &Channel<ImportEvent>, err: IpcError| -> IpcError {
        let _ = ch.send(ImportEvent::Failed {
            message: err.message.clone(),
        });
        err
    };

    // The session is locked for the whole import: introspection, optional
    // CREATE TABLE, and the inserts all use the one connection exclusively
    // (serializing with any concurrent query on the same session, as in v0.1).
    let mut sessions = state.sessions.lock().await;
    let session = match sessions.get_mut(&session_id) {
        Some(s) => s,
        None => return Err(fail(&on_progress, IpcError::unknown_session(&session_id))),
    };

    // Importing is a write; refuse it on a read-only connection (defence in
    // depth alongside the SQL guard).
    if session.read_only {
        return Err(fail(
            &on_progress,
            IpcError::new(
                "read_only",
                "this connection is read-only; importing is disabled",
            ),
        ));
    }

    let (import_target, dest) =
        match prepare_target(&mut *session.conn, &target, &mapping, &cancel).await {
            Ok(v) => v,
            Err(e) => return Err(fail(&on_progress, e)),
        };

    let csv_source = match CsvRowSource::open(&file, &core_opts, dest, BATCH_SIZE) {
        Ok(s) => s,
        Err(e) => return Err(fail(&on_progress, e.into())),
    };
    let mut source = ProgressSource {
        inner: csv_source,
        channel: on_progress.clone(),
        rows: 0,
    };

    let inserted = match session
        .conn
        .import_rows(
            &import_target,
            &mut source,
            core_opts.atomic,
            BATCH_SIZE,
            &cancel,
        )
        .await
    {
        Ok(n) => n,
        Err(e) => return Err(fail(&on_progress, e.into())),
    };

    let skipped = source.inner.rows_skipped();
    let _ = on_progress.send(ImportEvent::Done { inserted, skipped });
    tracing::info!(%session_id, inserted, skipped, "import finished");
    Ok(ImportSummary {
        rows_inserted: inserted,
        rows_skipped: skipped,
    })
}

/// Resolve the destination columns + per-column logical types, creating the
/// table first when importing into a new one.
async fn prepare_target(
    conn: &mut dyn Connection,
    target: &ImportTargetArg,
    mapping: &[ColumnMappingArg],
    cancel: &CancelToken,
) -> Result<(ImportTarget, Vec<DestColumn>), IpcError> {
    // Destination column name → its CSV source field (None = insert NULL).
    let csv_for: HashMap<&str, Option<usize>> = mapping
        .iter()
        .map(|m| (m.target_column.as_str(), m.csv_index))
        .collect();

    match target {
        ImportTargetArg::New {
            database,
            schema,
            table,
            columns,
        } => {
            if columns.is_empty() {
                return Err(IpcError::new(
                    "import",
                    "no columns defined for the new table",
                ));
            }
            let new_cols: Vec<NewColumn> = columns
                .iter()
                .map(|c| NewColumn {
                    name: c.name.clone(),
                    sql_type: c.sql_type.clone(),
                    nullable: c.nullable,
                })
                .collect();
            conn.create_table(database.as_deref(), schema, table, &new_cols, cancel)
                .await?;

            let dest = columns
                .iter()
                .map(|c| DestColumn {
                    csv_index: csv_for.get(c.name.as_str()).copied().flatten(),
                    logical: logical_for_sql_type(&c.sql_type),
                })
                .collect();
            let import_target = ImportTarget {
                database: database.clone(),
                schema: schema.clone(),
                table: table.clone(),
                columns: columns.iter().map(|c| c.name.clone()).collect(),
            };
            Ok((import_target, dest))
        }

        ImportTargetArg::Existing {
            database,
            schema,
            table,
        } => {
            // Introspection needs an explicit database; fall back to the
            // connection's current one when the caller did not pin it.
            let db = match database.clone() {
                Some(d) if !d.is_empty() => d,
                _ => conn.current_database().await?,
            };
            let cols = conn.list_columns(&db, schema, table).await?;
            let type_of: HashMap<&str, &str> = cols
                .iter()
                .map(|c| (c.name.as_str(), c.data_type.as_str()))
                .collect();

            // Build the destination in mapping order; every mapped column must
            // exist on the table.
            let mut dest = Vec::with_capacity(mapping.len());
            let mut names = Vec::with_capacity(mapping.len());
            for m in mapping {
                let data_type = type_of.get(m.target_column.as_str()).ok_or_else(|| {
                    IpcError::new(
                        "import",
                        format!(
                            "column '{}' does not exist in {schema}.{table}",
                            m.target_column
                        ),
                    )
                })?;
                dest.push(DestColumn {
                    csv_index: m.csv_index,
                    logical: logical_for_sql_type(data_type),
                });
                names.push(m.target_column.clone());
            }
            if names.is_empty() {
                return Err(IpcError::new("import", "no columns mapped"));
            }
            let import_target = ImportTarget {
                database: database.clone(),
                schema: schema.clone(),
                table: table.clone(),
                columns: names,
            };
            Ok((import_target, dest))
        }
    }
}

/// A [`RowSource`] that wraps the CSV source and emits
/// [`ImportEvent::Progress`] after each non-empty batch — the import mirror of
/// the export's `ProgressSink`. Skip-count is read from `inner` at the end.
struct ProgressSource {
    inner: CsvRowSource,
    channel: Channel<ImportEvent>,
    /// Running count of rows read (≈ rows about to be inserted).
    rows: u64,
}

#[async_trait]
impl RowSource for ProgressSource {
    async fn next_batch(&mut self) -> Result<Vec<Vec<CellValue>>, CoreError> {
        let batch = self.inner.next_batch().await?;
        if !batch.is_empty() {
            self.rows += batch.len() as u64;
            // A dropped listener just means no more progress UI; keep importing.
            let _ = self.channel.send(ImportEvent::Progress { rows: self.rows });
        }
        Ok(batch)
    }
}
