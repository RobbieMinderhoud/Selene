//! JSON export backend.
//!
//! Emits a JSON array of row objects keyed by column name:
//! `[{"id":1,"name":"a"},{"id":2,"name":"b"}]`.
//!
//! ## Streaming
//! Rows are written to the file as they arrive — we emit the opening `[`, then
//! each row object separated by commas, then the closing `]` at
//! [`finish`](JsonExporter::finish). No `Vec` of all rows is ever held in
//! memory.
//!
//! ## Key order
//! Objects are built **manually, field-by-field in column order**, rather than
//! via [`serde_json::Map`]. Without the `preserve_order` feature a `Map` is a
//! `BTreeMap` and would alphabetise keys; constructing the object text directly
//! guarantees the output column order matches the result set. Individual values
//! are still serialised with `serde_json` for correct escaping.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use serde_json::Value;

use crate::error::CoreError;
use crate::value::{CellValue, Column};

use super::bytes_to_hex;

/// Streams rows to a JSON-array file.
pub(super) struct JsonExporter {
    writer: BufWriter<File>,
    /// Column names captured from the header, used as object keys per row.
    columns: Vec<String>,
    /// `false` until the first row object is written, so we know whether to
    /// prefix the next object with a comma.
    wrote_any_row: bool,
    /// `true` once the opening `[` has been written.
    opened: bool,
}

impl JsonExporter {
    /// Create (or truncate) the JSON file at `path`.
    pub(super) fn create(path: &Path) -> Result<Self, CoreError> {
        let file = File::create(path).map_err(io_err)?;
        Ok(Self {
            writer: BufWriter::new(file),
            columns: Vec::new(),
            wrote_any_row: false,
            opened: false,
        })
    }

    /// Record the column names and open the JSON array.
    pub(super) fn write_header(&mut self, columns: &[Column]) -> Result<(), CoreError> {
        self.columns = columns.iter().map(|c| c.name.clone()).collect();
        self.writer.write_all(b"[").map_err(io_err)?;
        self.opened = true;
        Ok(())
    }

    /// Write one row as a JSON object, comma-separated from the previous one.
    pub(super) fn write_row(&mut self, row: &[CellValue]) -> Result<(), CoreError> {
        // Defensive: if rows arrive before a header (shouldn't happen via the
        // driver contract), open the array with no known column names.
        if !self.opened {
            self.writer.write_all(b"[").map_err(io_err)?;
            self.opened = true;
        }

        if self.wrote_any_row {
            self.writer.write_all(b",").map_err(io_err)?;
        }

        self.writer.write_all(b"{").map_err(io_err)?;
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                self.writer.write_all(b",").map_err(io_err)?;
            }
            // Key: the column name, or a positional fallback if the row is wider
            // than the header (keeps malformed input lossless rather than
            // panicking).
            let key = self
                .columns
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("column_{i}"));
            // Serialise the key as a JSON string for correct escaping.
            let key_json = Value::String(key).to_string();
            self.writer.write_all(key_json.as_bytes()).map_err(io_err)?;
            self.writer.write_all(b":").map_err(io_err)?;

            let value_json = cell_to_json(cell).to_string();
            self.writer
                .write_all(value_json.as_bytes())
                .map_err(io_err)?;
        }
        self.writer.write_all(b"}").map_err(io_err)?;

        self.wrote_any_row = true;
        Ok(())
    }

    /// Close the array and flush.
    pub(super) fn finish(mut self) -> Result<(), CoreError> {
        // If no header ever arrived (empty result), still produce a valid `[]`.
        if !self.opened {
            self.writer.write_all(b"[").map_err(io_err)?;
        }
        self.writer.write_all(b"]").map_err(io_err)?;
        self.writer.flush().map_err(io_err)?;
        Ok(())
    }
}

/// Map a [`CellValue`] to a [`serde_json::Value`].
///
/// - `Null` → `null`
/// - `Bool` → JSON bool
/// - `I64` → JSON number
/// - `F64` → JSON number when finite; **non-finite** (`NaN`/±∞, which JSON
///   cannot represent) → its string form
/// - `Decimal` / `String` / `Uuid` / `Unsupported` → JSON string
/// - `Bytes` → `0x`-prefixed hex string
/// - `DateTime` → the ISO string
pub(super) fn cell_to_json(value: &CellValue) -> Value {
    match value {
        CellValue::Null => Value::Null,
        CellValue::Bool(b) => Value::Bool(*b),
        CellValue::I64(n) => Value::Number((*n).into()),
        CellValue::F64(f) => {
            // `Number::from_f64` returns None for NaN/Infinity; fall back to a
            // string so the document stays valid JSON.
            match serde_json::Number::from_f64(*f) {
                Some(n) => Value::Number(n),
                None => Value::String(f.to_string()),
            }
        }
        CellValue::Decimal(s) => Value::String(s.clone()),
        CellValue::String(s) => Value::String(s.clone()),
        CellValue::Bytes(bytes) => Value::String(bytes_to_hex(bytes)),
        CellValue::DateTime { iso, .. } => Value::String(iso.clone()),
        CellValue::Uuid(s) => Value::String(s.clone()),
        // Nested document/array cells already hold JSON text: parse it back so it
        // is inlined as real JSON structure rather than a quoted string. On a
        // parse failure (pathological input only) fall back to the raw string so
        // the document stays valid.
        CellValue::Document(s) | CellValue::Array(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))
        }
        CellValue::Unsupported { text, .. } => Value::String(text.clone()),
    }
}

/// Map a low-level I/O error.
fn io_err(e: std::io::Error) -> CoreError {
    CoreError::Io(format!("JSON file I/O failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_floats_are_numbers_nonfinite_are_strings() {
        assert_eq!(cell_to_json(&CellValue::F64(1.5)), serde_json::json!(1.5));
        assert_eq!(
            cell_to_json(&CellValue::F64(f64::NAN)),
            Value::String("NaN".to_string())
        );
        assert_eq!(
            cell_to_json(&CellValue::F64(f64::INFINITY)),
            Value::String("inf".to_string())
        );
    }

    #[test]
    fn decimal_stays_a_string_to_preserve_precision() {
        assert_eq!(
            cell_to_json(&CellValue::Decimal("12345.6789".into())),
            Value::String("12345.6789".to_string())
        );
    }

    #[test]
    fn bytes_become_hex_string() {
        assert_eq!(
            cell_to_json(&CellValue::Bytes(vec![0x00, 0xff])),
            Value::String("0x00ff".to_string())
        );
    }
}
