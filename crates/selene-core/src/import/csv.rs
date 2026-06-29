//! CSV reading: a [`csv::Reader`] over a buffered file, exposed both as a
//! streaming [`RowSource`] for the import and as a one-shot [`analyze`] helper
//! that previews the header + a sample and infers per-column types.
//!
//! A leading UTF-8 BOM (common in Excel-exported CSVs, especially on Windows) is
//! stripped before parsing so the first header/field is never polluted with the
//! `\u{FEFF}` marker. Non-UTF-8 files (e.g. ISO-8859-1 / Windows-1252 from
//! Excel on Windows) are detected with `chardetng` and transcoded to UTF-8
//! in memory via `encoding_rs` before being passed to the CSV reader.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read};
use std::path::Path;

use async_trait::async_trait;

use crate::driver::RowSource;
use crate::error::CoreError;
use crate::value::{CellValue, LogicalType};

use super::{coerce_cell, infer_type, CsvAnalysis, CsvImportOptions};

/// One destination column's import plan: which CSV field feeds it (or `None` to
/// insert `NULL`) and the logical type its values are coerced into.
#[derive(Clone, Copy, Debug)]
pub struct DestColumn {
    /// Index of the source CSV field, or `None` to always insert `NULL`.
    pub csv_index: Option<usize>,
    /// The logical type to coerce the field into.
    pub logical: LogicalType,
}

/// Streams typed rows from a CSV file in destination-column order, ready to be
/// pulled by a driver's import path.
pub struct CsvRowSource {
    reader: csv::Reader<Box<dyn io::Read + Send>>,
    dest: Vec<DestColumn>,
    empty_as_null: bool,
    atomic: bool,
    batch_size: usize,
    record: csv::StringRecord,
    skipped: u64,
    done: bool,
}

impl CsvRowSource {
    /// Open `path` and prepare to stream rows for the given destination columns.
    /// `batch_size` is the maximum number of rows yielded per [`next_batch`]
    /// call (clamped to ≥ 1).
    ///
    /// [`next_batch`]: RowSource::next_batch
    pub fn open(
        path: &Path,
        opts: &CsvImportOptions,
        dest: Vec<DestColumn>,
        batch_size: usize,
    ) -> Result<Self, CoreError> {
        Ok(Self {
            reader: build_reader(path, opts)?,
            dest,
            empty_as_null: opts.empty_as_null,
            atomic: opts.atomic,
            batch_size: batch_size.max(1),
            record: csv::StringRecord::new(),
            skipped: 0,
            done: false,
        })
    }

    /// Rows skipped so far due to coercion failures (skip mode only).
    pub fn rows_skipped(&self) -> u64 {
        self.skipped
    }

    /// Coerce one parsed CSV record into a destination-ordered row. A mapped
    /// field that is missing on a short row is treated as empty.
    fn coerce(
        dest: &[DestColumn],
        record: &csv::StringRecord,
        empty_as_null: bool,
    ) -> Result<Vec<CellValue>, CoreError> {
        dest.iter()
            .map(|d| {
                let text = d.csv_index.and_then(|i| record.get(i)).unwrap_or("");
                coerce_cell(text, d.logical, empty_as_null)
            })
            .collect()
    }
}

#[async_trait]
impl RowSource for CsvRowSource {
    async fn next_batch(&mut self) -> Result<Vec<Vec<CellValue>>, CoreError> {
        if self.done {
            return Ok(Vec::new());
        }
        let mut batch: Vec<Vec<CellValue>> = Vec::with_capacity(self.batch_size);
        // Fill the batch with *good* rows. In skip mode a bad row is dropped and
        // does not count toward the batch, so the loop keeps reading until it has
        // `batch_size` good rows or reaches EOF — an empty return therefore
        // always means "done", never "this batch happened to be all-bad".
        while batch.len() < self.batch_size {
            if !self.reader.read_record(&mut self.record).map_err(csv_err)? {
                self.done = true;
                break;
            }
            match Self::coerce(&self.dest, &self.record, self.empty_as_null) {
                Ok(row) => batch.push(row),
                Err(e) => {
                    if self.atomic {
                        return Err(e);
                    }
                    self.skipped += 1;
                }
            }
        }
        Ok(batch)
    }
}

/// Read a CSV's header plus up to `sample_limit` data rows and infer a type per
/// column. Used by the mapping menu before an import is configured.
pub(super) fn analyze(
    path: &Path,
    opts: &CsvImportOptions,
    sample_limit: usize,
) -> Result<CsvAnalysis, CoreError> {
    // A handful of raw lines so the UI can show the file verbatim (delimiter and
    // all), independent of how `opts` happens to be set right now.
    let raw_preview = read_raw_preview(path, 5)?;

    let mut reader = build_reader(path, opts)?;

    // With a header, `headers()` consumes the first record as names; without
    // one, every record (including the first) is data and we synthesise names.
    let header_names: Vec<String> = if opts.has_header {
        reader
            .headers()
            .map_err(csv_err)?
            .iter()
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };

    let mut sample_rows: Vec<Vec<String>> = Vec::new();
    let mut record = csv::StringRecord::new();
    while sample_rows.len() < sample_limit && reader.read_record(&mut record).map_err(csv_err)? {
        sample_rows.push(record.iter().map(str::to_string).collect());
    }

    let col_count = header_names
        .len()
        .max(sample_rows.iter().map(Vec::len).max().unwrap_or(0));

    let headers: Vec<String> = (0..col_count)
        .map(|i| {
            header_names
                .get(i)
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("column_{}", i + 1))
        })
        .collect();

    let inferred = (0..col_count)
        .map(|c| {
            let samples: Vec<&str> = sample_rows
                .iter()
                .filter_map(|r| r.get(c).map(String::as_str))
                .collect();
            infer_type(&samples)
        })
        .collect();

    Ok(CsvAnalysis {
        headers,
        sample_rows,
        inferred,
        raw_preview,
    })
}

/// Read up to `limit` raw lines from `path` (BOM stripped, line terminators
/// dropped, encoding-corrected), for a verbatim file preview.
fn read_raw_preview(path: &Path, limit: usize) -> Result<Vec<String>, CoreError> {
    let inner = detect_and_open(path)?;
    let buf = BufReader::new(inner);
    let mut out = Vec::with_capacity(limit);
    for line in buf.lines() {
        if out.len() >= limit {
            break;
        }
        out.push(line.map_err(io_err)?);
    }
    Ok(out)
}

/// Open `path`, auto-detect its character encoding, and return a `Read`er
/// positioned at the start of the content (BOM consumed, non-UTF-8 transcoded).
///
/// * UTF-8 / ASCII → reopen as `BufReader<File>`, strip the 3-byte UTF-8 BOM
///   if present. No extra allocation.
/// * Any other encoding → read the whole file, decode to UTF-8 with
///   `encoding_rs`, strip any BOM from the decoded text, return a
///   `Cursor<Vec<u8>>` over the UTF-8 bytes.
fn detect_and_open(path: &Path) -> Result<Box<dyn io::Read + Send>, CoreError> {
    const SAMPLE: usize = 8 * 1024;
    let mut sample_buf = vec![0u8; SAMPLE];
    let sample_len = {
        let mut f = File::open(path).map_err(io_err)?;
        f.read(&mut sample_buf).map_err(io_err)?
    };
    let sample = &sample_buf[..sample_len];

    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    // `last = true` when the whole file fit in the sample buffer.
    detector.feed(sample, sample_len < SAMPLE);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);

    if encoding == encoding_rs::UTF_8 {
        let mut buf = BufReader::new(File::open(path).map_err(io_err)?);
        {
            let peeked = buf.fill_buf().map_err(io_err)?;
            if peeked.starts_with(&[0xEF, 0xBB, 0xBF]) {
                buf.consume(3);
            }
        }
        return Ok(Box::new(buf));
    }

    let raw = std::fs::read(path).map_err(io_err)?;
    let (cow, _enc, had_errors) = encoding.decode(&raw);
    if had_errors {
        return Err(CoreError::Import(format!(
            "CSV file contains bytes that cannot be decoded as {} (detected encoding)",
            encoding.name()
        )));
    }
    let text = cow.strip_prefix('\u{FEFF}').unwrap_or(&cow);
    Ok(Box::new(Cursor::new(text.as_bytes().to_vec())))
}

/// Build a CSV reader from `opts`. Encoding detection and BOM stripping are
/// handled by [`detect_and_open`]. `flexible` tolerates ragged rows.
fn build_reader(
    path: &Path,
    opts: &CsvImportOptions,
) -> Result<csv::Reader<Box<dyn io::Read + Send>>, CoreError> {
    let reader = detect_and_open(path)?;
    Ok(csv::ReaderBuilder::new()
        .delimiter(opts.delimiter)
        .quote(opts.quote)
        .quoting(opts.quoting)
        .has_headers(opts.has_header)
        .flexible(true)
        .from_reader(reader))
}

fn csv_err(e: csv::Error) -> CoreError {
    CoreError::Import(format!("CSV read failed: {e}"))
}

fn io_err(e: std::io::Error) -> CoreError {
    CoreError::Io(format!("CSV file I/O failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_csv(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    fn opts() -> CsvImportOptions {
        CsvImportOptions {
            delimiter: b',',
            ..Default::default()
        }
    }

    #[test]
    fn analyze_reads_header_and_infers() {
        let f = write_csv("id,name,amount\n1,Alice,12.50\n2,Bob,8.00\n");
        let a = analyze(f.path(), &opts(), 100).unwrap();
        assert_eq!(a.headers, vec!["id", "name", "amount"]);
        assert_eq!(a.sample_rows.len(), 2);
        assert_eq!(a.inferred[0].logical, LogicalType::Integer);
        assert_eq!(a.inferred[1].logical, LogicalType::Text);
        assert_eq!(a.inferred[2].logical, LogicalType::Decimal);
        // The raw preview shows verbatim lines (delimiter visible).
        assert_eq!(a.raw_preview[0], "id,name,amount");
        assert_eq!(a.raw_preview.len(), 3);
    }

    #[test]
    fn analyze_strips_bom_and_synthesises_headers() {
        // Leading BOM + no header row.
        let f = write_csv("\u{FEFF}1,x\n2,y\n");
        let o = CsvImportOptions {
            has_header: false,
            ..opts()
        };
        let a = analyze(f.path(), &o, 10).unwrap();
        assert_eq!(a.headers, vec!["column_1", "column_2"]);
        // BOM must not pollute the first field — it still infers as integer.
        assert_eq!(a.inferred[0].logical, LogicalType::Integer);
        assert_eq!(a.sample_rows.len(), 2);
    }

    #[tokio::test]
    async fn row_source_streams_coerced_rows() {
        let f = write_csv("id,amount\n1,1.5\n2,2.5\n3,3.5\n");
        let dest = vec![
            DestColumn {
                csv_index: Some(0),
                logical: LogicalType::Integer,
            },
            DestColumn {
                csv_index: Some(1),
                logical: LogicalType::Decimal,
            },
        ];
        let mut src = CsvRowSource::open(f.path(), &opts(), dest, 2).unwrap();

        let first = src.next_batch().await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(
            first[0],
            vec![CellValue::I64(1), CellValue::Decimal("1.5".into())]
        );
        let second = src.next_batch().await.unwrap();
        assert_eq!(second.len(), 1);
        let third = src.next_batch().await.unwrap();
        assert!(third.is_empty(), "empty batch signals done");
        assert_eq!(src.rows_skipped(), 0);
    }

    #[tokio::test]
    async fn row_source_skip_mode_drops_bad_rows() {
        // Row 2 has a non-integer id; skip mode keeps the other two.
        let f = write_csv("id\n1\noops\n3\n");
        let o = CsvImportOptions {
            atomic: false,
            ..opts()
        };
        let dest = vec![DestColumn {
            csv_index: Some(0),
            logical: LogicalType::Integer,
        }];
        let mut src = CsvRowSource::open(f.path(), &o, dest, 10).unwrap();
        let batch = src.next_batch().await.unwrap();
        assert_eq!(
            batch,
            vec![vec![CellValue::I64(1)], vec![CellValue::I64(3)]]
        );
        assert_eq!(src.rows_skipped(), 1);
        assert!(src.next_batch().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn row_source_atomic_mode_errors_on_bad_row() {
        let f = write_csv("id\n1\noops\n");
        let dest = vec![DestColumn {
            csv_index: Some(0),
            logical: LogicalType::Integer,
        }];
        let mut src = CsvRowSource::open(f.path(), &opts(), dest, 10).unwrap();
        let err = src.next_batch().await.unwrap_err();
        assert!(matches!(err, CoreError::Import(_)));
    }

    #[tokio::test]
    async fn unmapped_column_inserts_null() {
        let f = write_csv("a\nx\n");
        let dest = vec![
            DestColumn {
                csv_index: Some(0),
                logical: LogicalType::Text,
            },
            // No CSV source → always NULL.
            DestColumn {
                csv_index: None,
                logical: LogicalType::Integer,
            },
        ];
        let mut src = CsvRowSource::open(f.path(), &opts(), dest, 10).unwrap();
        let batch = src.next_batch().await.unwrap();
        assert_eq!(
            batch,
            vec![vec![CellValue::String("x".into()), CellValue::Null]]
        );
    }
}
