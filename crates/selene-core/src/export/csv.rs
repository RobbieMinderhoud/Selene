//! CSV export backend.
//!
//! Wraps a [`csv::Writer`] over a buffered file. The header row is the column
//! names; each data row renders its cells via
//! [`cell_to_text`](super::cell_to_text). The `csv` crate handles RFC-4180
//! quoting/escaping, so values containing the delimiter, quotes, or newlines are
//! emitted safely regardless of the configured quote style.

use std::fs::File;
use std::io::Write as _;
use std::path::Path;

use crate::error::CoreError;
use crate::value::{CellValue, Column};

use super::{cell_to_text, CsvLineEnding, CsvOptions, CsvQuoteStyle};

/// Streams rows to a CSV file.
pub(super) struct CsvExporter {
    writer: csv::Writer<File>,
    include_header: bool,
}

impl CsvExporter {
    /// Create (or truncate) the CSV file at `path`, applying all `opts`.
    pub(super) fn create(path: &Path, opts: CsvOptions) -> Result<Self, CoreError> {
        let mut file = File::create(path).map_err(io_err)?;

        if opts.bom {
            // UTF-8 BOM lets Excel open the file without a re-encoding prompt.
            file.write_all(&[0xEF, 0xBB, 0xBF]).map_err(io_err)?;
        }

        let terminator = match opts.line_ending {
            CsvLineEnding::Crlf => csv::Terminator::CRLF,
            CsvLineEnding::Lf => csv::Terminator::Any(b'\n'),
        };

        let quote_style = match opts.quote_style {
            CsvQuoteStyle::Necessary => csv::QuoteStyle::Necessary,
            CsvQuoteStyle::Always => csv::QuoteStyle::Always,
            CsvQuoteStyle::NonNumeric => csv::QuoteStyle::NonNumeric,
            CsvQuoteStyle::Never => csv::QuoteStyle::Never,
        };

        let writer = csv::WriterBuilder::new()
            .delimiter(opts.delimiter)
            .quote(opts.quote)
            .quote_style(quote_style)
            .terminator(terminator)
            .from_writer(file);

        Ok(Self {
            writer,
            include_header: opts.include_header,
        })
    }

    /// Write the header row from the column names.
    pub(super) fn write_header(&mut self, columns: &[Column]) -> Result<(), CoreError> {
        if !self.include_header {
            return Ok(());
        }
        let names = columns.iter().map(|c| c.name.as_str());
        self.writer.write_record(names).map_err(csv_err)
    }

    /// Write one data row.
    pub(super) fn write_row(&mut self, row: &[CellValue]) -> Result<(), CoreError> {
        let fields: Vec<String> = row.iter().map(cell_to_text).collect();
        self.writer.write_record(&fields).map_err(csv_err)
    }

    /// Flush the buffered writer, ensuring all bytes hit the file.
    pub(super) fn finish(mut self) -> Result<(), CoreError> {
        self.writer.flush().map_err(io_err)
    }
}

fn csv_err(e: csv::Error) -> CoreError {
    CoreError::Export(format!("CSV write failed: {e}"))
}

fn io_err(e: std::io::Error) -> CoreError {
    CoreError::Io(format!("CSV file I/O failed: {e}"))
}
