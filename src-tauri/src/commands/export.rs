//! Streaming export-to-file command.
//!
//! [`export_result`] runs a query and writes its first result set straight to
//! disk via the core [`Exporter`] (itself a [`RowSink`]), so a multi-million-row
//! export never materialises in memory. It reuses the exact
//! [`Connection::execute`](selene_core::Connection::execute) path the data grid
//! uses; the only addition here is forwarding periodic progress to the
//! frontend over a `tauri::ipc::Channel`.
//!
//! Unlike [`query_run`](super::query::query_run), this command **awaits** the
//! export to completion before returning the [`ExportSummary`] — the frontend
//! drives it with a progress channel and a single awaited result.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use tauri::ipc::Channel;
use tauri::State;

use selene_core::{
    CellValue, Column, CsvLineEnding, CsvOptions, CsvQuoteStyle, ExecOptions, ExportFormat,
    ExportSummary, Exporter, Flow, RowSink,
};

use crate::commands::ExportEvent;
use crate::error::IpcError;
use crate::state::AppState;

/// Rows buffered per batch before flushing; also the progress-emission cadence.
const BATCH_SIZE: usize = 500;

/// CSV export options received from the frontend (snake_case fields; all
/// optional so omitted keys fall back to sensible defaults).
#[derive(Deserialize, Default)]
pub(crate) struct CsvExportOptions {
    delimiter: Option<String>,
    quote: Option<String>,
    quote_style: Option<String>,
    line_ending: Option<String>,
    include_header: Option<bool>,
    bom: Option<bool>,
}

impl CsvExportOptions {
    pub(crate) fn into_core(self) -> CsvOptions {
        CsvOptions {
            delimiter: first_byte(self.delimiter, b';'),
            quote: first_byte(self.quote, b'"'),
            quote_style: match self.quote_style.as_deref() {
                Some("always") => CsvQuoteStyle::Always,
                Some("non_numeric") => CsvQuoteStyle::NonNumeric,
                Some("never") => CsvQuoteStyle::Never,
                _ => CsvQuoteStyle::Necessary,
            },
            line_ending: match self.line_ending.as_deref() {
                Some("lf") => CsvLineEnding::Lf,
                _ => CsvLineEnding::Crlf,
            },
            include_header: self.include_header.unwrap_or(true),
            bom: self.bom.unwrap_or(false),
        }
    }
}

fn first_byte(s: Option<String>, fallback: u8) -> u8 {
    s.as_deref()
        .and_then(|v| v.as_bytes().first().copied())
        .unwrap_or(fallback)
}

/// A [`RowSink`] that writes through to an [`Exporter`] while emitting
/// [`ExportEvent::Progress`] after each row batch.
///
/// The inner `Exporter` records the authoritative written-row count and
/// surfaces any write error from `finish`; this wrapper only adds progress
/// reporting. A failed `channel.send` (listener gone) stops the stream early.
///
/// The `Channel` is concrete, so no runtime generic is needed; the sink stays
/// `Send` as `RowSink` requires.
struct ProgressSink {
    exporter: Exporter,
    channel: Channel<ExportEvent>,
    /// Running count of rows handed to the exporter, for progress events.
    rows: u64,
}

impl ProgressSink {
    fn new(exporter: Exporter, channel: Channel<ExportEvent>) -> Self {
        Self {
            exporter,
            channel,
            rows: 0,
        }
    }
}

#[async_trait]
impl RowSink for ProgressSink {
    async fn on_meta(&mut self, set_index: usize, columns: Vec<Column>) -> Flow {
        self.exporter.on_meta(set_index, columns).await
    }

    async fn on_rows(&mut self, set_index: usize, rows: Vec<Vec<CellValue>>) -> Flow {
        // Only the first result set is exported (a flat file has nowhere to put
        // later sets); count progress for that set only, matching the Exporter.
        let batch = if set_index == 0 { rows.len() as u64 } else { 0 };
        let flow = self.exporter.on_rows(set_index, rows).await;
        if batch > 0 {
            self.rows += batch;
            if self
                .channel
                .send(ExportEvent::Progress { rows: self.rows })
                .is_err()
            {
                return Flow::Stop;
            }
        }
        flow
    }

    async fn on_set_end(&mut self, set_index: usize, affected: Option<u64>) -> Flow {
        self.exporter.on_set_end(set_index, affected).await
    }
}

/// Run a query and export its first result set to `path` in `format`.
///
/// Progress is streamed as [`ExportEvent::Progress`]; the command emits a final
/// [`ExportEvent::Done`] (also returning the [`ExportSummary`]) on success, or
/// [`ExportEvent::Failed`] and an `Err` on failure. A fresh
/// [`CancelToken`](selene_core::CancelToken) backs the run; cancellation of an
/// export is not wired to a separate command in v0.1 (the run is awaited).
// A Tauri command surface: each argument maps to a JS IPC field, so the count
// is dictated by the command contract rather than refactorable into a struct.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn export_result(
    state: State<'_, AppState>,
    session_id: String,
    sql: String,
    format: ExportFormat,
    path: String,
    max_rows: Option<u64>,
    // CSV-specific options; ignored for JSON/XLSX.
    csv_options: Option<CsvExportOptions>,
    on_progress: Channel<ExportEvent>,
) -> Result<ExportSummary, IpcError> {
    let target = PathBuf::from(&path);
    let opts = ExecOptions {
        max_rows,
        batch_size: BATCH_SIZE,
    };
    let csv_opts = csv_options.unwrap_or_default().into_core();

    // A failure helper that also notifies the frontend before returning Err, so
    // the progress channel always sees a terminal event.
    let fail = |on_progress: &Channel<ExportEvent>, err: IpcError| -> IpcError {
        let _ = on_progress.send(ExportEvent::Failed {
            message: err.message.clone(),
        });
        err
    };

    let exporter = match Exporter::create(format, &target, csv_opts) {
        Ok(e) => e,
        Err(e) => return Err(fail(&on_progress, e.into())),
    };

    let cancel = selene_core::CancelToken::new();
    let mut sink = ProgressSink::new(exporter, on_progress.clone());

    // Execute into the progress sink. The session is locked for the duration,
    // serializing with any concurrent query on the same session (v0.1).
    let exec_result = {
        let mut sessions = state.sessions.lock().await;
        match sessions.get_mut(&session_id) {
            Some(session) => session.conn.execute(&sql, &opts, &mut sink, &cancel).await,
            None => {
                return Err(fail(&on_progress, IpcError::unknown_session(&session_id)));
            }
        }
    };

    // Recover the exporter to flush/close it. `finish` re-raises any write error
    // stashed mid-stream, so it must be called (and checked) regardless of how
    // `execute` returned — but if `execute` itself errored, surface that first.
    let ProgressSink { exporter, .. } = sink;

    if let Err(err) = exec_result {
        // Drop the exporter's file work; report the execution error.
        let _ = exporter.finish();
        return Err(fail(&on_progress, err.into()));
    }

    let summary = match exporter.finish() {
        Ok(summary) => summary,
        Err(e) => return Err(fail(&on_progress, e.into())),
    };

    let _ = on_progress.send(ExportEvent::Done {
        rows: summary.rows_written,
    });
    tracing::info!(
        %session_id,
        format = ?format,
        rows = summary.rows_written,
        "export finished"
    );
    Ok(summary)
}
