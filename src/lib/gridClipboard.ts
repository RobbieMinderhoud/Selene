/**
 * Build the clipboard payload for a copy from the results grid, in the format
 * the user picked (Settings → Results) or chose via the right-click menu.
 *
 * The builder is pure: it takes the already-stringified column headers and a
 * rectangle of cell strings, so it's trivially unit-testable. Cell-text
 * extraction (`copyCellText`) and the actual `navigator.clipboard` write
 * (`writeClipboard`) are kept separate from it.
 *
 * Format notes:
 *  - **tab** — TSV. Excel/Sheets split a pasted line into columns on TAB only
 *    (a comma stays inside one cell), so this is the spreadsheet-friendly default.
 *  - **comma** — RFC-4180-ish CSV: a field is quoted when it contains a comma,
 *    quote, or newline, and embedded quotes are doubled.
 *  - **markdown** — a GitHub-flavoured pipe table. A markdown table is invalid
 *    without a header + separator row, so this format ALWAYS emits the header,
 *    regardless of `includeHeaders`.
 *  - **html** — a real `<table>`, written to the clipboard as a `text/html`
 *    flavour (with a TSV `text/plain` fallback) so it pastes as a formatted
 *    table into Excel/Word/Sheets.
 */

import type { CellValue } from "../ipc/types";
import { formatCell } from "./cellFormat";
import type { CopyFormat } from "../state/settingsStore";

/** A cell's copy text. NULL copies as an empty cell (Excel reads it as blank). */
export function copyCellText(value: CellValue | undefined): string {
  if (!value) return "";
  const f = formatCell(value);
  return f.isNull ? "" : f.text;
}

function csvField(s: string): string {
  return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
}

function mdField(s: string): string {
  // Escape backslash first, then pipe (the cell separator), and flatten newlines
  // so a multi-line value can't break the single-line table row.
  return s
    .replace(/\\/g, "\\\\")
    .replace(/\|/g, "\\|")
    .replace(/\r?\n/g, "<br>");
}

function htmlField(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

export interface ClipboardPayload {
  /** Always present; the plain-text flavour (and the only flavour for non-HTML). */
  text: string;
  /** Set only for the `html` format — written as a `text/html` clipboard flavour. */
  html?: string;
}

/**
 * Build the payload from `headers` (one per selected column, in order) and a
 * `matrix` of row-major cell strings. `includeHeaders` governs tab/comma/html;
 * markdown always includes the header (the format requires it).
 */
export function buildClipboard(
  format: CopyFormat,
  headers: string[],
  matrix: string[][],
  includeHeaders: boolean,
): ClipboardPayload {
  switch (format) {
    case "comma": {
      const lines: string[] = [];
      if (includeHeaders) lines.push(headers.map(csvField).join(","));
      for (const row of matrix) lines.push(row.map(csvField).join(","));
      return { text: lines.join("\r\n") };
    }
    case "markdown": {
      const head = `| ${headers.map(mdField).join(" | ")} |`;
      const sep = `| ${headers.map(() => "---").join(" | ")} |`;
      const body = matrix.map((row) => `| ${row.map(mdField).join(" | ")} |`);
      return { text: [head, sep, ...body].join("\n") };
    }
    case "html": {
      const thead = includeHeaders
        ? `<thead><tr>${headers
            .map((h) => `<th>${htmlField(h)}</th>`)
            .join("")}</tr></thead>`
        : "";
      const tbody = `<tbody>${matrix
        .map(
          (row) =>
            `<tr>${row.map((c) => `<td>${htmlField(c)}</td>`).join("")}</tr>`,
        )
        .join("")}</tbody>`;
      // Plain-text fallback mirrors the TSV format.
      const lines: string[] = [];
      if (includeHeaders) lines.push(headers.join("\t"));
      for (const row of matrix) lines.push(row.join("\t"));
      return {
        text: lines.join("\r\n"),
        html: `<table>${thead}${tbody}</table>`,
      };
    }
    case "tab":
    default: {
      const lines: string[] = [];
      if (includeHeaders) lines.push(headers.join("\t"));
      for (const row of matrix) lines.push(row.join("\t"));
      return { text: lines.join("\r\n") };
    }
  }
}

/** Write a payload to the clipboard, using `text/html` when the format has it. */
export async function writeClipboard(payload: ClipboardPayload): Promise<void> {
  if (
    payload.html &&
    typeof ClipboardItem !== "undefined" &&
    navigator.clipboard?.write
  ) {
    await navigator.clipboard.write([
      new ClipboardItem({
        "text/html": new Blob([payload.html], { type: "text/html" }),
        "text/plain": new Blob([payload.text], { type: "text/plain" }),
      }),
    ]);
    return;
  }
  await navigator.clipboard.writeText(payload.text);
}
