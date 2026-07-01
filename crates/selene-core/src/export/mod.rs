//! Streaming result-set exporters (CSV / JSON / XLSX).
//!
//! ## Why a [`RowSink`]
//! Exporters implement [`RowSink`](crate::driver::RowSink), the very trait a
//! driver streams into. That means an export reuses the exact
//! [`Connection::execute`](crate::driver::Connection::execute) path the data
//! grid uses — rows are written to disk batch-by-batch as they arrive, so a
//! multi-million-row export never has to materialise in memory.
//!
//! ## v0.1 limitation: first result set only
//! A SQL batch can yield several result sets. A flat file (one CSV / one JSON
//! array / one worksheet) has nowhere to put the 2nd, 3rd, … set, so this
//! version exports **only the first result set (`set_index == 0`)** and
//! silently ignores later ones. Multi-set export (e.g. one worksheet per set)
//! is left for a future revision.
//!
//! ## Value formatting
//! CSV and XLSX share [`cell_to_text`]; JSON uses its own
//! [`CellValue`](crate::value::CellValue) → [`serde_json::Value`] mapping (see
//! [`json`]). CSV and JSON keep [`CellValue::Decimal`] as its exact text so
//! financial precision is never lost; XLSX writes decimals as native number
//! cells (parsing to `f64`) so Excel can sum and filter them.

mod csv;
mod json;
mod xlsx;

use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::driver::{Flow, RowSink};
use crate::error::CoreError;
use crate::value::{CellValue, Column};

/// The on-disk format an export targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// RFC-4180 CSV (UTF-8, with a header row).
    Csv,
    /// A JSON array of row objects keyed by column name.
    Json,
    /// A native Excel `.xlsx` workbook with a single worksheet.
    Xlsx,
}

/// Controls when fields are quoted in a CSV export.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CsvQuoteStyle {
    /// Quote only fields that contain the delimiter, quote character, or a newline.
    /// This is the RFC-4180 default.
    #[default]
    Necessary,
    /// Always quote every field.
    Always,
    /// Quote every non-numeric field.
    NonNumeric,
    /// Never quote fields. Values containing the delimiter will be corrupt.
    Never,
}

/// Line terminator written after each CSV record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CsvLineEnding {
    /// `\r\n` — the Windows and RFC-4180 standard. Required for Excel on Windows.
    #[default]
    Crlf,
    /// `\n` — Unix line endings.
    Lf,
}

/// All user-configurable options for a CSV export.
#[derive(Clone, Debug)]
pub struct CsvOptions {
    /// Field separator byte (e.g. `b';'`, `b','`, `b'\t'`, `b'|'`).
    pub delimiter: u8,
    /// Character used to quote fields that contain special characters.
    pub quote: u8,
    /// Determines which fields are quoted.
    pub quote_style: CsvQuoteStyle,
    /// Line terminator written after each record.
    pub line_ending: CsvLineEnding,
    /// Whether to write column names as the first row.
    pub include_header: bool,
    /// Prepend a UTF-8 BOM (`\xEF\xBB\xBF`) so Excel opens the file without a
    /// re-encoding prompt on Windows.
    pub bom: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: b';',
            quote: b'"',
            quote_style: CsvQuoteStyle::Necessary,
            line_ending: CsvLineEnding::Crlf,
            include_header: true,
            bom: false,
        }
    }
}

/// Outcome of a completed export.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSummary {
    /// Number of data rows written (header rows excluded).
    pub rows_written: u64,
}

/// A format-agnostic handle that owns its output file and writes rows as they
/// stream in.
///
/// Construct with [`Exporter::create`], feed it as a
/// [`RowSink`](crate::driver::RowSink) to `Connection::execute`, then call
/// [`Exporter::finish`] to flush and close (XLSX is actually written to disk at
/// `finish`). Only the first result set is recorded; see the module docs.
///
/// ## Surfacing write errors
/// The [`RowSink`](crate::driver::RowSink) callbacks can only return a
/// [`Flow`], not a `Result`, so a write failure mid-stream cannot propagate
/// through the trait. Instead the **first** such error is stashed in
/// `failure`, further writes are skipped, and [`Flow::Stop`] ends the stream;
/// [`finish`](Exporter::finish) then returns that stored error. A caller's
/// contract is therefore: drive the sink, then *always* call `finish` and
/// check its `Result` — a `Flow::Stop` alone does not imply success.
pub struct Exporter {
    inner: Backend,
    /// Total data rows written so far.
    rows_written: u64,
    /// `true` once result set 0's metadata has been seen. Late sets and any
    /// rows arriving before metadata are ignored.
    started: bool,
    /// The first error encountered while writing header/rows, if any. Held here
    /// because the sink trait cannot return it; re-raised by `finish`.
    failure: Option<CoreError>,
}

/// The per-format writer state. The CSV and XLSX writers carry sizeable inline
/// state (an 8 KiB `csv::Writer`, a whole `Workbook`); both are boxed so every
/// variant — and thus every `Exporter` — stays pointer-sized and balanced.
enum Backend {
    Csv(Box<csv::CsvExporter>),
    Json(json::JsonExporter),
    Xlsx(Box<xlsx::XlsxExporter>),
}

impl Exporter {
    /// Open `path` for writing and prepare an exporter for `format`.
    ///
    /// CSV/JSON create (or truncate) the file immediately; XLSX builds an
    /// in-memory workbook that is written out at [`finish`](Exporter::finish).
    ///
    /// `csv_options` controls how the CSV is written (delimiter, quoting, BOM,
    /// etc.) and is ignored for JSON and XLSX.
    pub fn create(
        format: ExportFormat,
        path: &Path,
        csv_options: CsvOptions,
    ) -> Result<Self, CoreError> {
        let inner = match format {
            ExportFormat::Csv => {
                Backend::Csv(Box::new(csv::CsvExporter::create(path, csv_options)?))
            }
            ExportFormat::Json => Backend::Json(json::JsonExporter::create(path)?),
            ExportFormat::Xlsx => Backend::Xlsx(Box::new(xlsx::XlsxExporter::create(path)?)),
        };
        Ok(Self {
            inner,
            rows_written: 0,
            started: false,
            failure: None,
        })
    }

    /// Flush and close the output, returning a summary. For XLSX this is where
    /// the workbook is serialised to the path.
    ///
    /// If a write failed earlier in the stream, that original error is returned
    /// here in preference to attempting a (likely also-failing) flush, so the
    /// caller sees the root cause.
    pub fn finish(mut self) -> Result<ExportSummary, CoreError> {
        // A failure recorded during streaming takes precedence: report it
        // rather than masking it with a downstream flush error.
        if let Some(err) = self.failure.take() {
            return Err(err);
        }
        match self.inner {
            Backend::Csv(e) => e.finish()?,
            Backend::Json(e) => e.finish()?,
            Backend::Xlsx(e) => e.finish()?,
        }
        Ok(ExportSummary {
            rows_written: self.rows_written,
        })
    }
}

#[async_trait]
impl RowSink for Exporter {
    async fn on_meta(&mut self, set_index: usize, columns: Vec<Column>) -> Flow {
        // If a prior write already failed, stop immediately.
        if self.failure.is_some() {
            return Flow::Stop;
        }
        // Only the first result set is exported; record its header once.
        if set_index == 0 && !self.started {
            self.started = true;
            if let Err(e) = self.write_header(&columns) {
                // Stash the error for `finish` to surface, then stop the stream.
                self.failure = Some(e);
                return Flow::Stop;
            }
        }
        Flow::Continue
    }

    async fn on_rows(&mut self, set_index: usize, rows: Vec<Vec<CellValue>>) -> Flow {
        if self.failure.is_some() {
            return Flow::Stop;
        }
        // Ignore rows for any set other than the first, and any rows that
        // somehow precede metadata.
        if set_index != 0 || !self.started {
            return Flow::Continue;
        }
        for row in &rows {
            if let Err(e) = self.write_row(row) {
                self.failure = Some(e);
                return Flow::Stop;
            }
            self.rows_written += 1;
        }
        Flow::Continue
    }

    async fn on_set_end(&mut self, _set_index: usize, _affected: Option<u64>) -> Flow {
        // Nothing to do: CSV/JSON flush at `finish`, XLSX saves at `finish`.
        Flow::Continue
    }
}

impl Exporter {
    /// Dispatch a header write to the active backend.
    fn write_header(&mut self, columns: &[Column]) -> Result<(), CoreError> {
        match &mut self.inner {
            Backend::Csv(e) => e.write_header(columns),
            Backend::Json(e) => e.write_header(columns),
            Backend::Xlsx(e) => e.write_header(columns),
        }
    }

    /// Dispatch a row write to the active backend.
    fn write_row(&mut self, row: &[CellValue]) -> Result<(), CoreError> {
        match &mut self.inner {
            Backend::Csv(e) => e.write_row(row),
            Backend::Json(e) => e.write_row(row),
            Backend::Xlsx(e) => e.write_row(row),
        }
    }
}

/// Render a [`CellValue`] as plain text for the CSV and XLSX (string-cell)
/// exporters.
///
/// - `Null` → empty string
/// - `Bool` → `"true"` / `"false"`
/// - `I64` / `F64` → the number's natural text form
/// - `Decimal` → its stored text verbatim (exact precision preserved)
/// - `String` / `Uuid` → as-is
/// - `Bytes` → lowercase hex with a `0x` prefix
/// - `DateTime` → the ISO string as stored
/// - `Unsupported` → its preserved `text`
pub fn cell_to_text(value: &CellValue) -> String {
    match value {
        CellValue::Null => String::new(),
        CellValue::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => f.to_string(),
        CellValue::Decimal(s) => s.clone(),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(bytes) => bytes_to_hex(bytes),
        CellValue::DateTime { iso, .. } => iso.clone(),
        CellValue::Uuid(s) => s.clone(),
        // Nested document/array cells carry their JSON text; render it verbatim.
        CellValue::Document(s) | CellValue::Array(s) => s.clone(),
        CellValue::Unsupported { text, .. } => text.clone(),
    }
}

/// Encode bytes as `0x`-prefixed lowercase hex (e.g. `&[0xDE, 0xAD]` → `0xdead`).
pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    // 2 hex chars per byte + the "0x" prefix. Build directly from a nibble
    // lookup so a large binary cell doesn't allocate a tiny `String` per byte.
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::TemporalKind;

    #[test]
    fn cell_to_text_covers_every_variant() {
        assert_eq!(cell_to_text(&CellValue::Null), "");
        assert_eq!(cell_to_text(&CellValue::Bool(true)), "true");
        assert_eq!(cell_to_text(&CellValue::Bool(false)), "false");
        assert_eq!(cell_to_text(&CellValue::I64(-42)), "-42");
        assert_eq!(cell_to_text(&CellValue::F64(1.5)), "1.5");
        // Decimal text is preserved exactly, trailing/leading digits intact.
        assert_eq!(
            cell_to_text(&CellValue::Decimal("12345.6789".into())),
            "12345.6789"
        );
        assert_eq!(cell_to_text(&CellValue::String("hi".into())), "hi");
        assert_eq!(
            cell_to_text(&CellValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef])),
            "0xdeadbeef"
        );
        assert_eq!(
            cell_to_text(&CellValue::DateTime {
                iso: "2026-06-15T12:00:00".into(),
                kind: TemporalKind::DateTime,
            }),
            "2026-06-15T12:00:00"
        );
        assert_eq!(
            cell_to_text(&CellValue::Uuid(
                "550e8400-e29b-41d4-a716-446655440000".into()
            )),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(
            cell_to_text(&CellValue::Unsupported {
                type_name: "geography".into(),
                text: "POINT(0 0)".into(),
            }),
            "POINT(0 0)"
        );
    }

    #[test]
    fn empty_bytes_render_as_bare_prefix() {
        assert_eq!(bytes_to_hex(&[]), "0x");
    }

    #[test]
    fn export_format_serde_is_lowercase() {
        assert_eq!(
            serde_json::to_string(&ExportFormat::Csv).unwrap(),
            "\"csv\""
        );
        assert_eq!(
            serde_json::to_string(&ExportFormat::Xlsx).unwrap(),
            "\"xlsx\""
        );
        let f: ExportFormat = serde_json::from_str("\"json\"").unwrap();
        assert_eq!(f, ExportFormat::Json);
    }
}

/// Golden, end-to-end tests that drive the full [`RowSink`] lifecycle of an
/// [`Exporter`] over a temp file and assert the produced bytes exactly. These
/// run under `#[tokio::test]` because the sink methods are async.
#[cfg(test)]
mod golden_tests {
    use super::*;
    use crate::value::{LogicalType, TemporalKind};
    use std::fs;
    use std::io::Read;
    use tempfile::NamedTempFile;

    /// Build a column with sensible defaults; only the name matters for the
    /// exporters (logical type is incidental here).
    fn col(name: &str, ordinal: usize, logical: LogicalType) -> Column {
        Column {
            name: name.to_string(),
            ordinal,
            db_type: "test".to_string(),
            logical,
            nullable: Some(true),
        }
    }

    /// The shared fixture: a small result set exercising Null, exact Decimal
    /// precision, Bytes (incl. empty), DateTime, and Unsupported.
    fn fixture() -> (Vec<Column>, Vec<Vec<CellValue>>) {
        let columns = vec![
            col("id", 0, LogicalType::Integer),
            col("amount", 1, LogicalType::Decimal),
            col("blob", 2, LogicalType::Binary),
            col("ts", 3, LogicalType::DateTime),
            col("geo", 4, LogicalType::Other),
        ];
        let rows = vec![
            vec![
                CellValue::I64(1),
                CellValue::Decimal("12345.6789".into()),
                CellValue::Bytes(vec![0xde, 0xad]),
                CellValue::DateTime {
                    iso: "2026-06-15T12:00:00".into(),
                    kind: TemporalKind::DateTime,
                },
                CellValue::Null,
            ],
            vec![
                CellValue::Null,
                CellValue::Decimal("-0.5".into()),
                CellValue::Bytes(vec![]),
                CellValue::DateTime {
                    iso: "2026-06-15".into(),
                    kind: TemporalKind::Date,
                },
                CellValue::Unsupported {
                    type_name: "geography".into(),
                    text: "POINT(0 0)".into(),
                },
            ],
        ];
        (columns, rows)
    }

    /// CsvOptions for tests: comma delimiter, LF line endings (keeps expected
    /// strings clean — no `\r\n` in string literals).
    fn test_csv_opts() -> CsvOptions {
        CsvOptions {
            delimiter: b',',
            line_ending: CsvLineEnding::Lf,
            ..Default::default()
        }
    }

    /// Drive the full sink lifecycle for the fixture and finish, returning the
    /// summary. The exporter writes to `path`.
    async fn run_export(format: ExportFormat, path: &std::path::Path) -> ExportSummary {
        let (columns, rows) = fixture();
        let mut exporter =
            Exporter::create(format, path, test_csv_opts()).expect("create exporter");

        assert_eq!(exporter.on_meta(0, columns).await, Flow::Continue);
        assert_eq!(exporter.on_rows(0, rows).await, Flow::Continue);
        assert_eq!(exporter.on_set_end(0, None).await, Flow::Continue);

        exporter.finish().expect("finish export")
    }

    fn read_to_string(path: &std::path::Path) -> String {
        let mut s = String::new();
        fs::File::open(path)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    }

    #[tokio::test]
    async fn csv_golden_bytes() {
        let tmp = NamedTempFile::new().unwrap();
        let summary = run_export(ExportFormat::Csv, tmp.path()).await;
        assert_eq!(summary.rows_written, 2);

        let got = read_to_string(tmp.path());
        // Default csv writer: LF terminator, fields quoted only when necessary.
        // None of the fixture values contain a comma/quote/newline, so nothing
        // is quoted. Null → empty field; Bytes → 0x hex; empty Bytes → "0x".
        let expected = "\
id,amount,blob,ts,geo
1,12345.6789,0xdead,2026-06-15T12:00:00,
,-0.5,0x,2026-06-15,POINT(0 0)
";
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn json_golden_bytes() {
        let tmp = NamedTempFile::new().unwrap();
        let summary = run_export(ExportFormat::Json, tmp.path()).await;
        assert_eq!(summary.rows_written, 2);

        let got = read_to_string(tmp.path());
        // Array of objects, keys in column order. Decimal stays a string for
        // precision; Null → null; Bytes → hex string; I64 → number.
        let expected = concat!(
            "[",
            r#"{"id":1,"amount":"12345.6789","blob":"0xdead","ts":"2026-06-15T12:00:00","geo":null}"#,
            ",",
            r#"{"id":null,"amount":"-0.5","blob":"0x","ts":"2026-06-15","geo":"POINT(0 0)"}"#,
            "]"
        );
        assert_eq!(got, expected);

        // And it must be valid, re-parseable JSON.
        let parsed: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn xlsx_finishes_and_writes_a_nonempty_file() {
        let tmp = NamedTempFile::new().unwrap();
        let summary = run_export(ExportFormat::Xlsx, tmp.path()).await;
        assert_eq!(summary.rows_written, 2);

        // A valid .xlsx is a non-trivial ZIP container; just assert it is
        // non-empty and begins with the ZIP local-file-header magic "PK\x03\x04".
        let bytes = fs::read(tmp.path()).unwrap();
        assert!(!bytes.is_empty(), "xlsx file should not be empty");
        assert!(
            bytes.starts_with(b"PK\x03\x04"),
            "xlsx should be a ZIP container"
        );
    }

    #[tokio::test]
    async fn empty_result_set_produces_valid_output() {
        // Header only, no rows: CSV is just the header line, JSON is `[]`.
        let columns = vec![col("a", 0, LogicalType::Integer)];

        let csv_tmp = NamedTempFile::new().unwrap();
        let mut csv_exp =
            Exporter::create(ExportFormat::Csv, csv_tmp.path(), test_csv_opts()).unwrap();
        csv_exp.on_meta(0, columns.clone()).await;
        csv_exp.on_set_end(0, None).await;
        let csv_summary = csv_exp.finish().unwrap();
        assert_eq!(csv_summary.rows_written, 0);
        assert_eq!(read_to_string(csv_tmp.path()), "a\n");

        let json_tmp = NamedTempFile::new().unwrap();
        let mut json_exp =
            Exporter::create(ExportFormat::Json, json_tmp.path(), test_csv_opts()).unwrap();
        json_exp.on_meta(0, columns).await;
        json_exp.on_set_end(0, None).await;
        let json_summary = json_exp.finish().unwrap();
        assert_eq!(json_summary.rows_written, 0);
        assert_eq!(read_to_string(json_tmp.path()), "[]");
    }

    #[tokio::test]
    async fn only_first_result_set_is_exported() {
        // Set 0 contributes rows; set 1's meta/rows are ignored (v0.1 limit).
        let columns0 = vec![col("a", 0, LogicalType::Integer)];
        let columns1 = vec![col("b", 0, LogicalType::Integer)];

        let tmp = NamedTempFile::new().unwrap();
        let mut exp = Exporter::create(ExportFormat::Csv, tmp.path(), test_csv_opts()).unwrap();

        exp.on_meta(0, columns0).await;
        exp.on_rows(0, vec![vec![CellValue::I64(1)]]).await;
        exp.on_set_end(0, None).await;

        // Second set: must be entirely ignored.
        exp.on_meta(1, columns1).await;
        exp.on_rows(1, vec![vec![CellValue::I64(99)]]).await;
        exp.on_set_end(1, None).await;

        let summary = exp.finish().unwrap();
        assert_eq!(summary.rows_written, 1, "only set 0's row counts");
        assert_eq!(read_to_string(tmp.path()), "a\n1\n");
    }

    #[tokio::test]
    async fn mid_stream_write_error_is_surfaced_by_finish() {
        // A row wider than Excel's 16,384-column limit makes the XLSX backend's
        // `write_row` fail. The sink can only return `Flow::Stop`, so the error
        // must be stashed and re-raised by `finish` — proving write failures are
        // observable rather than silently swallowed.
        let tmp = NamedTempFile::new().unwrap();
        let mut exp = Exporter::create(ExportFormat::Xlsx, tmp.path(), test_csv_opts()).unwrap();

        // One header column is fine.
        let header = vec![col("a", 0, LogicalType::Integer)];
        assert_eq!(exp.on_meta(0, header).await, Flow::Continue);

        // An over-wide row: 16_385 cells (max valid index is 16_383).
        let wide_row: Vec<CellValue> = (0..16_385).map(|_| CellValue::I64(1)).collect();
        assert_eq!(
            exp.on_rows(0, vec![wide_row]).await,
            Flow::Stop,
            "an over-wide row must stop the stream"
        );

        // Once failed, further callbacks keep stopping.
        assert_eq!(
            exp.on_rows(0, vec![vec![CellValue::I64(2)]]).await,
            Flow::Stop
        );

        let err = exp
            .finish()
            .expect_err("finish must surface the stored error");
        assert!(
            matches!(err, CoreError::Export(_)),
            "expected Export error, got {err:?}"
        );
    }
}
