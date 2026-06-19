/**
 * Render a {@link CellValue} to a display string + a coarse class for the grid.
 *
 * Rules (per STEP 6a):
 *  - Null      -> dim "NULL"
 *  - Bytes     -> `0x<hex>` (uppercase hex, truncated for very long blobs)
 *  - DateTime  -> the ISO string
 *  - Unsupported -> its preserved text
 *  - objects   -> text
 * Numbers are right-aligned; everything else left-aligned.
 */

import type { CellValue } from "../ipc/types";

export type CellAlign = "left" | "right";

export interface FormattedCell {
  text: string;
  /** True for SQL NULL (rendered dim/italic). */
  isNull: boolean;
  align: CellAlign;
}

/** Max bytes to render inline as hex before truncating with an ellipsis. */
const MAX_HEX_BYTES = 32;

function bytesToHex(bytes: number[]): string {
  const shown = bytes.slice(0, MAX_HEX_BYTES);
  let hex = "0x";
  for (const b of shown) hex += b.toString(16).padStart(2, "0").toUpperCase();
  if (bytes.length > MAX_HEX_BYTES) hex += `… (${bytes.length} bytes)`;
  return hex;
}

export function formatCell(value: CellValue): FormattedCell {
  switch (value.t) {
    case "Null":
      return { text: "NULL", isNull: true, align: "left" };
    case "Bool":
      return { text: value.v ? "true" : "false", isNull: false, align: "left" };
    case "I64":
      return { text: String(value.v), isNull: false, align: "right" };
    case "F64":
      return { text: String(value.v), isNull: false, align: "right" };
    case "Decimal":
      return { text: value.v, isNull: false, align: "right" };
    case "String":
      return { text: value.v, isNull: false, align: "left" };
    case "Bytes":
      return { text: bytesToHex(value.v), isNull: false, align: "left" };
    case "DateTime":
      return { text: value.v.iso, isNull: false, align: "left" };
    case "Uuid":
      return { text: value.v, isNull: false, align: "left" };
    case "Unsupported":
      return { text: value.v.text, isNull: false, align: "left" };
    default: {
      // Exhaustiveness guard: if a new variant is added, fall back to JSON.
      const exhaustive: never = value;
      return { text: JSON.stringify(exhaustive), isNull: false, align: "left" };
    }
  }
}
