//! XLSX export backend.
//!
//! Builds a native `.xlsx` workbook with one worksheet via
//! [`rust_xlsxwriter`]. Unlike CSV/JSON, the workbook is assembled in memory
//! and serialised to disk in [`finish`](XlsxExporter::finish) — the library has
//! no incremental-to-disk writer, so "streaming" here means we still consume
//! the [`RowSink`](crate::driver::RowSink) row-by-row without holding our own
//! row buffer, but the library buffers the sheet until save.
//!
//! Numeric cells (`I64`/`F64`/`Decimal`) are written as real numbers so they
//! sort and compute correctly in Excel. `DateTime` cells are written as native
//! Excel date/datetime cells with a `yyyy-mm-dd`/`hh:mm:ss` number format.
//! Everything else is written as a string via
//! [`cell_to_text`](super::cell_to_text).

use std::path::{Path, PathBuf};

use rust_xlsxwriter::{ExcelDateTime, Format, Workbook};

use crate::error::CoreError;
use crate::value::{CellValue, Column, TemporalKind};

use super::cell_to_text;

/// `rust_xlsxwriter` row index type (`u32`).
type RowNum = u32;
/// `rust_xlsxwriter` column index type (`u16`).
type ColNum = u16;

/// Builds an `.xlsx` workbook and saves it on `finish`.
pub(super) struct XlsxExporter {
    workbook: Workbook,
    /// Destination path; the workbook is written here at `finish`.
    path: PathBuf,
    /// Bold format for the header row.
    header_format: Format,
    /// Number format for date-only cells.
    date_format: Format,
    /// Number format for datetime (and datetime-with-offset) cells.
    datetime_format: Format,
    /// Number format for time-only cells.
    time_format: Format,
    /// Next worksheet row to write (0 = header row). Tracked here because the
    /// borrow of the worksheet is taken per-write.
    next_row: RowNum,
}

impl XlsxExporter {
    /// Prepare an in-memory workbook with a single worksheet.
    pub(super) fn create(path: &Path) -> Result<Self, CoreError> {
        let mut workbook = Workbook::new();
        // Add the single worksheet up front so later writes target it by index.
        workbook.add_worksheet();
        Ok(Self {
            workbook,
            path: path.to_path_buf(),
            header_format: Format::new().set_bold(),
            date_format: Format::new().set_num_format("yyyy-mm-dd"),
            datetime_format: Format::new().set_num_format("yyyy-mm-dd hh:mm:ss"),
            time_format: Format::new().set_num_format("hh:mm:ss"),
            next_row: 0,
        })
    }

    /// Write the bold header row from the column names.
    pub(super) fn write_header(&mut self, columns: &[Column]) -> Result<(), CoreError> {
        let row = self.next_row;
        // Clone the format out first: `worksheet()` borrows `self` mutably, so
        // we cannot also hold a shared borrow of `self.header_format` across the
        // write. `Format` is cheap to clone.
        let header_format = self.header_format.clone();
        let sheet = self.worksheet()?;
        for (i, col) in columns.iter().enumerate() {
            let c = col_index(i)?;
            sheet
                .write_with_format(row, c, col.name.as_str(), &header_format)
                .map_err(xlsx_err)?;
        }
        self.next_row += 1;
        Ok(())
    }

    /// Write one data row: integers and decimals as numbers, datetimes as
    /// native date cells, and everything else as text.
    pub(super) fn write_row(&mut self, row: &[CellValue]) -> Result<(), CoreError> {
        let r = self.next_row;
        // Clone temporal formats before the mutable worksheet borrow so both
        // can be used inside the loop without a borrow conflict. Format is cheap
        // to clone (it holds a small struct).
        let date_fmt = self.date_format.clone();
        let datetime_fmt = self.datetime_format.clone();
        let time_fmt = self.time_format.clone();
        let sheet = self.worksheet()?;
        for (i, cell) in row.iter().enumerate() {
            let c = col_index(i)?;
            match cell {
                // Native integer: write as f64 so Excel treats it as a number.
                // Values beyond 2^53 lose integer precision — an accepted
                // spreadsheet limitation.
                CellValue::I64(n) => {
                    sheet.write_number(r, c, *n as f64).map_err(xlsx_err)?;
                }
                CellValue::F64(f) if f.is_finite() => {
                    sheet.write_number(r, c, *f).map_err(xlsx_err)?;
                }
                // Exact numerics: parse to f64 for a native number cell so
                // Excel can sum, average, and filter them. Financial values
                // (premiums, costs, taxes) have at most a few decimal places
                // and stay within f64's 15–17 significant-digit range. On parse
                // failure — pathological input only — fall back to text.
                CellValue::Decimal(s) => {
                    if let Ok(f) = s.parse::<f64>() {
                        sheet.write_number(r, c, f).map_err(xlsx_err)?;
                    } else {
                        sheet.write_string(r, c, s.as_str()).map_err(xlsx_err)?;
                    }
                }
                // Temporal values: write as native Excel date cells so Excel
                // sorts and filters them correctly. DateTimeOffset timezone info
                // is stripped (Excel has no timezone concept). If the ISO string
                // cannot be parsed, fall back to text.
                CellValue::DateTime { iso, kind } => {
                    let fmt = match kind {
                        TemporalKind::Date => &date_fmt,
                        TemporalKind::Time => &time_fmt,
                        TemporalKind::DateTime | TemporalKind::DateTimeOffset => &datetime_fmt,
                    };
                    let normalised = strip_tz_offset(iso);
                    match ExcelDateTime::parse_from_str(normalised) {
                        Ok(dt) => {
                            sheet.write_with_format(r, c, &dt, fmt).map_err(xlsx_err)?;
                        }
                        Err(_) => {
                            sheet.write_string(r, c, iso.as_str()).map_err(xlsx_err)?;
                        }
                    }
                }
                // Everything else (non-finite floats, strings, bytes, etc.) as text.
                other => {
                    sheet
                        .write_string(r, c, cell_to_text(other))
                        .map_err(xlsx_err)?;
                }
            }
        }
        self.next_row += 1;
        Ok(())
    }

    /// Serialise the workbook to its path.
    pub(super) fn finish(mut self) -> Result<(), CoreError> {
        self.workbook.save(&self.path).map_err(xlsx_err)
    }

    /// Borrow the (only) worksheet by index 0.
    fn worksheet(&mut self) -> Result<&mut rust_xlsxwriter::Worksheet, CoreError> {
        self.workbook.worksheet_from_index(0).map_err(xlsx_err)
    }
}

/// Strip a trailing `+HH:MM` or `-HH:MM` timezone offset from an ISO-8601
/// datetime string. `ExcelDateTime::parse_from_str` handles the `Z` suffix
/// itself (it's one of its split characters) but not numeric offsets — without
/// stripping, the `+HH` part ends up absorbed into the seconds token and
/// silently zeroes it out.
fn strip_tz_offset(iso: &str) -> &str {
    if iso.len() > 6 {
        let (prefix, tail) = iso.split_at(iso.len() - 6);
        let b = tail.as_bytes();
        if (b[0] == b'+' || b[0] == b'-')
            && b[1].is_ascii_digit()
            && b[2].is_ascii_digit()
            && b[3] == b':'
            && b[4].is_ascii_digit()
            && b[5].is_ascii_digit()
        {
            return prefix;
        }
    }
    iso
}

/// Convert a `usize` column ordinal to the library's `u16`, erroring past the
/// Excel column limit (16,384 columns) rather than truncating silently.
fn col_index(i: usize) -> Result<ColNum, CoreError> {
    ColNum::try_from(i).map_err(|_| {
        CoreError::Export(format!(
            "column index {i} exceeds the spreadsheet maximum of {} columns",
            ColNum::MAX
        ))
    })
}

/// Map a `rust_xlsxwriter` error to [`CoreError::Export`].
fn xlsx_err(e: rust_xlsxwriter::XlsxError) -> CoreError {
    CoreError::Export(format!("XLSX write failed: {e}"))
}
