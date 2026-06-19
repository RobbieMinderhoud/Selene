/**
 * Virtualized result grid: TanStack **Table** (column model) + TanStack
 * **react-virtual** (windowing).
 *
 * ## Why TanStack (not Glide Data Grid)
 * Glide Data Grid 6.x peer-deps cap at React 18 (`^16.12.0 || 17.x || 18.x`) and
 * this project is on React 19.1, so it does not install/run cleanly here.
 * TanStack Table + react-virtual are both MIT and explicitly React-19-safe, and
 * keep the bundle lean (no canvas / lodash / marked deps). The table builds the
 * column definitions + sizing/header model; react-virtual windows BOTH rows and
 * columns with absolute positioning over a single scroll container, so a
 * 50k-row × wide result stays smooth.
 *
 * ## How streaming drives it
 * The component subscribes to the active result set's `rev` counter (bumped on
 * every append). The table is used for the **column model** (definitions,
 * sizing, header rendering); the **body cells are read directly from the
 * in-place-mutated `resultSet.rows` buffer**, indexed by the virtualizer. This
 * is deliberate: `getCoreRowModel` memoizes its row model on the `data`
 * *reference*, which never changes when we append in place — so driving the body
 * off the buffer (re-rendered when `rev` bumps) both avoids that stale-cache
 * trap and skips per-row `createRow` overhead at 50k+ rows. The editor never
 * re-renders on append because it subscribes to a different slice (the `sql`).
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  createColumnHelper,
  flexRender,
  getCoreRowModel,
  useReactTable,
} from "@tanstack/react-table";
import type { ColumnDef } from "@tanstack/react-table";
import { useVirtualizer } from "@tanstack/react-virtual";

import type { CellValue, Column } from "../ipc/types";
import { formatCell } from "../lib/cellFormat";
import type { ResultSet } from "../state/editorStore";
import { useSettingsStore, DENSITY_TO_PX } from "../state/settingsStore";
import styles from "./ResultsGrid.module.css";

interface ResultsGridProps {
  resultSet: ResultSet;
  /** Bumped on every store mutation; forces a re-read of the row buffer. */
  rev: number;
}

/** A grid row is just the array of cells for that record. */
type GridRow = CellValue[];

/** Stable empty `data` for the column-only table instance (see comment below). */
const EMPTY_DATA: GridRow[] = [];

const HEADER_HEIGHT = 30;
const ROW_NUM_WIDTH = 56;
const MIN_COL_WIDTH = 96;
const MAX_COL_WIDTH = 420;
const CHAR_PX = 7.6;
// Extra scrollable width past the last column. On the macOS WebView (WKWebView)
// the rightmost column failed to paint when it sat exactly on the scroll
// boundary (it reappeared after scrolling back a touch — i.e. on any repaint).
// There's nothing to render past the last column, so overscan can't buffer it;
// instead we make the canvas a little wider than the columns so the last column
// is never flush against max-scroll. The strip is empty grid space (like the
// trailing space in a spreadsheet).
const TRAILING_PAD = 64;

const columnHelper = createColumnHelper<GridRow>();

/** Estimate a column's pixel width from its name + a sample of values. */
function estimateColWidth(
  name: string,
  rows: ResultSet["rows"],
  colIndex: number,
): number {
  let maxChars = name.length + 3;
  const sample = Math.min(rows.length, 40);
  for (let i = 0; i < sample; i++) {
    const cell = rows[i]?.[colIndex];
    if (!cell) continue;
    const len = formatCell(cell).text.length;
    if (len > maxChars) maxChars = len;
  }
  return Math.max(MIN_COL_WIDTH, Math.min(MAX_COL_WIDTH, maxChars * CHAR_PX));
}

/**
 * Build TanStack column defs (with sizes) from the result-set columns. Typed as
 * `ColumnDef<GridRow, CellValue>[]` so the accessor's `CellValue` value type
 * lines up with the helper output (the generic `ColumnDef<GridRow>` widens the
 * value to `unknown` and rejects the accessor's narrower type).
 */
function buildColumns(
  cols: Column[],
  rows: ResultSet["rows"],
): ColumnDef<GridRow, CellValue>[] {
  return cols.map((col, i) =>
    columnHelper.accessor((row) => row[i], {
      id: String(col.ordinal) + ":" + col.name,
      header: col.name,
      size: estimateColWidth(col.name, rows, i),
      meta: { dbType: col.db_type },
    }),
  );
}

function cellKey(r: number, c: number): string {
  return `${r}:${c}`;
}

export function ResultsGrid({ resultSet, rev }: ResultsGridProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const { columns, rows } = resultSet;

  const density = useSettingsStore((s) => s.results.density);
  const nullDisplay = useSettingsStore((s) => s.results.nullDisplay);
  const rowHeight = DENSITY_TO_PX[density];

  const [selectedCells, setSelectedCells] = useState<ReadonlySet<string>>(
    new Set(),
  );
  const [isCopied, setIsCopied] = useState(false);
  const flashTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const anchorRef = useRef<{ row: number; col: number } | null>(null);
  // Movable end of the keyboard selection (anchor is the fixed end).
  const cursorRef = useRef<{ row: number; col: number } | null>(null);
  // Scroll-to-index callbacks, set after the virtualizers are created below.
  const vRowScrollRef = useRef<((i: number) => void) | null>(null);
  const vColScrollRef = useRef<((i: number) => void) | null>(null);

  // Clear selection when the result set changes (new query).
  useEffect(() => {
    setSelectedCells(new Set());
    anchorRef.current = null;
    cursorRef.current = null;
  }, [resultSet]);

  // Cleanup the flash timeout on unmount.
  useEffect(
    () => () => {
      if (flashTimeoutRef.current) clearTimeout(flashTimeoutRef.current);
    },
    [],
  );

  const handleCellClick = useCallback(
    (rowIdx: number, colIdx: number, e: React.MouseEvent) => {
      e.stopPropagation();
      scrollRef.current?.focus({ preventScroll: true });
      const key = cellKey(rowIdx, colIdx);
      if (e.metaKey || e.ctrlKey) {
        setSelectedCells((prev) => {
          const next = new Set(prev);
          if (next.has(key)) {
            next.delete(key);
          } else {
            next.add(key);
            anchorRef.current = { row: rowIdx, col: colIdx };
            cursorRef.current = { row: rowIdx, col: colIdx };
          }
          return next;
        });
      } else if (e.shiftKey && anchorRef.current) {
        const { row: ar, col: ac } = anchorRef.current;
        const minRow = Math.min(ar, rowIdx);
        const maxRow = Math.max(ar, rowIdx);
        const minCol = Math.min(ac, colIdx);
        const maxCol = Math.max(ac, colIdx);
        const next = new Set<string>();
        for (let r = minRow; r <= maxRow; r++) {
          for (let c = minCol; c <= maxCol; c++) {
            next.add(cellKey(r, c));
          }
        }
        cursorRef.current = { row: rowIdx, col: colIdx };
        setSelectedCells(next);
      } else {
        setSelectedCells(new Set([key]));
        anchorRef.current = { row: rowIdx, col: colIdx };
        cursorRef.current = { row: rowIdx, col: colIdx };
      }
    },
    [],
  );

  const handleRowNumClick = useCallback(
    (rowIdx: number, e: React.MouseEvent) => {
      e.stopPropagation();
      scrollRef.current?.focus({ preventScroll: true });
      const next = new Set<string>();
      if (e.shiftKey && anchorRef.current) {
        const minRow = Math.min(anchorRef.current.row, rowIdx);
        const maxRow = Math.max(anchorRef.current.row, rowIdx);
        for (let r = minRow; r <= maxRow; r++) {
          for (let c = 0; c < columns.length; c++) {
            next.add(cellKey(r, c));
          }
        }
        cursorRef.current = { row: rowIdx, col: columns.length - 1 };
      } else {
        for (let c = 0; c < columns.length; c++) {
          next.add(cellKey(rowIdx, c));
        }
        anchorRef.current = { row: rowIdx, col: 0 };
        cursorRef.current = { row: rowIdx, col: columns.length - 1 };
      }
      setSelectedCells(next);
    },
    [columns.length],
  );

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "c" && selectedCells.size > 0) {
        e.preventDefault();
        const sorted = [...selectedCells]
          .map((k) => {
            const [r, c] = k.split(":").map(Number);
            return { r, c };
          })
          .sort((a, b) => (a.r !== b.r ? a.r - b.r : a.c - b.c));
        const byRow = new Map<number, number[]>();
        for (const { r, c } of sorted) {
          const cols = byRow.get(r) ?? [];
          cols.push(c);
          byRow.set(r, cols);
        }
        const rowTexts: string[] = [];
        for (const [r, cols] of [...byRow].sort((a, b) => a[0] - b[0])) {
          const cellTexts = cols.map((c) => {
            const val = rows[r]?.[c];
            if (!val) return "";
            const f = formatCell(val);
            return f.isNull ? "" : f.text;
          });
          rowTexts.push(cellTexts.join(", "));
        }
        navigator.clipboard.writeText(rowTexts.join("\n"));
        // Flash feedback: pulse the selected cells bright then settle.
        if (flashTimeoutRef.current) clearTimeout(flashTimeoutRef.current);
        setIsCopied(true);
        flashTimeoutRef.current = setTimeout(() => setIsCopied(false), 500);
        return;
      }

      // Shift+Arrow: extend selection from anchor toward cursor.
      if (
        e.shiftKey &&
        (e.key === "ArrowUp" ||
          e.key === "ArrowDown" ||
          e.key === "ArrowLeft" ||
          e.key === "ArrowRight")
      ) {
        const cursor = cursorRef.current;
        const anchor = anchorRef.current;
        if (!cursor || !anchor) return;
        e.preventDefault();

        let { row: newRow, col: newCol } = cursor;
        if (e.key === "ArrowUp") newRow = Math.max(0, newRow - 1);
        else if (e.key === "ArrowDown")
          newRow = Math.min(rows.length - 1, newRow + 1);
        else if (e.key === "ArrowLeft") newCol = Math.max(0, newCol - 1);
        else newCol = Math.min(columns.length - 1, newCol + 1);

        cursorRef.current = { row: newRow, col: newCol };

        const minRow = Math.min(anchor.row, newRow);
        const maxRow = Math.max(anchor.row, newRow);
        const minCol = Math.min(anchor.col, newCol);
        const maxCol = Math.max(anchor.col, newCol);
        const next = new Set<string>();
        for (let r = minRow; r <= maxRow; r++) {
          for (let c = minCol; c <= maxCol; c++) {
            next.add(cellKey(r, c));
          }
        }
        setSelectedCells(next);
        vRowScrollRef.current?.(newRow);
        vColScrollRef.current?.(newCol);
      }
    },
    [selectedCells, rows, columns.length],
  );

  // Column defs recomputed when columns change or more rows stream in (rev:
  // widths sample early rows). Cheap and capped.
  const columnDefs = useMemo(
    () => buildColumns(columns, rows),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [columns, rev],
  );

  // The table instance exists for the COLUMN model (sizing + header render)
  // only. Its `data` is intentionally empty — body cells are read straight from
  // the streaming `rows` buffer below, not from the table's row model.
  const table = useReactTable<GridRow>({
    data: EMPTY_DATA,
    columns: columnDefs,
    getCoreRowModel: getCoreRowModel(),
  });

  const leafColumns = table.getVisibleLeafColumns();

  // Precompute x-offsets (after the sticky row-number column).
  const colOffsets = useMemo(() => {
    const offsets: number[] = [ROW_NUM_WIDTH];
    for (let i = 0; i < leafColumns.length; i++) {
      offsets.push(offsets[i] + leafColumns[i].getSize());
    }
    return offsets;
  }, [leafColumns]);

  const totalWidth = colOffsets[colOffsets.length - 1] ?? ROW_NUM_WIDTH;
  // Scrollable canvas is a touch wider than the columns so the last column is
  // never flush against the max-scroll boundary (see TRAILING_PAD).
  const canvasWidth = totalWidth + TRAILING_PAD;

  const rowVirtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowHeight,
    overscan: 12,
  });

  const colVirtualizer = useVirtualizer({
    horizontal: true,
    count: leafColumns.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: (i) => leafColumns[i]?.getSize() ?? MIN_COL_WIDTH,
    // Keep a generous buffer of off-screen columns mounted on either side of
    // the viewport. Wide columns (long names hit MAX_COL_WIDTH) mean each
    // unmounted column is a large blank slot, and on slower paint paths (the
    // macOS WebView during a fast horizontal fling) a too-small buffer can
    // briefly show an unpainted column at the leading edge. 8 ≈ the 12 used
    // for rows, scaled for the wider per-item size.
    overscan: 8,
    // The virtualized columns start ROW_NUM_WIDTH into the canvas (after the
    // sticky row-number column). Without this offset, react-virtual's view of
    // scrollLeft is shifted left by ROW_NUM_WIDTH relative to where the columns
    // actually render, so a column unmounts as soon as its leftmost edge
    // crosses the viewport edge — leaving the last ~ROW_NUM_WIDTH px of it
    // blank until the next column scrolls in.
    scrollMargin: ROW_NUM_WIDTH,
  });

  vRowScrollRef.current = (i) =>
    rowVirtualizer.scrollToIndex(i, { align: "auto" });
  vColScrollRef.current = (i) =>
    colVirtualizer.scrollToIndex(i, { align: "auto" });

  const virtualRows = rowVirtualizer.getVirtualItems();
  const virtualCols = colVirtualizer.getVirtualItems();

  if (columns.length === 0) {
    return <div className={styles.empty}>No columns in this result set.</div>;
  }

  const headers = table.getHeaderGroups()[0]?.headers ?? [];

  return (
    <div
      ref={scrollRef}
      className={styles.scroll}
      tabIndex={0}
      onKeyDown={handleKeyDown}
      onClick={() => {
        setSelectedCells(new Set());
        anchorRef.current = null;
      }}
    >
      <div
        className={styles.canvas}
        style={{
          width: canvasWidth,
          height: HEADER_HEIGHT + rowVirtualizer.getTotalSize(),
        }}
      >
        {/* Sticky header */}
        <div
          className={styles.headerRow}
          style={{ width: canvasWidth, height: HEADER_HEIGHT }}
        >
          <div
            className={`${styles.headerCell} ${styles.rowNumHeader}`}
            style={{ width: ROW_NUM_WIDTH }}
          >
            #
          </div>
          {virtualCols.map((vc) => {
            const header = headers[vc.index];
            const col = leafColumns[vc.index];
            const dbType = (
              col.columnDef.meta as { dbType?: string } | undefined
            )?.dbType;
            return (
              <div
                key={col.id}
                className={styles.headerCell}
                style={{
                  left: colOffsets[vc.index],
                  width: col.getSize(),
                }}
                title={`${String(header?.column.columnDef.header ?? "")} · ${dbType ?? ""}`}
                onClick={(e) => e.stopPropagation()}
              >
                <span className={styles.headerName}>
                  {header
                    ? flexRender(
                        header.column.columnDef.header,
                        header.getContext(),
                      )
                    : null}
                </span>
                <span className={styles.headerType}>{dbType}</span>
              </div>
            );
          })}
        </div>

        {/* Virtualized body — cells read straight from the streaming buffer. */}
        {virtualRows.map((vr) => {
          const row = rows[vr.index];
          const zebra = vr.index % 2 === 1;
          return (
            <div
              key={vr.index}
              className={`${styles.row} ${zebra ? styles.zebra : ""}`}
              style={{
                transform: `translateY(${HEADER_HEIGHT + vr.start}px)`,
                height: vr.size,
                width: canvasWidth,
              }}
            >
              <div
                className={`${styles.cell} ${styles.rowNum}`}
                style={{ width: ROW_NUM_WIDTH }}
                onClick={(e) => handleRowNumClick(vr.index, e)}
              >
                {vr.index + 1}
              </div>
              {virtualCols.map((vc) => {
                const value = row?.[vc.index];
                const f = value
                  ? formatCell(value)
                  : { text: "", isNull: false, align: "left" as const };
                // Apply user-configured null display; keep original text in
                // the tooltip so the true "NULL" is always discoverable.
                const displayText = f.isNull ? nullDisplay : f.text;
                const isSelected = selectedCells.has(
                  cellKey(vr.index, vc.index),
                );
                return (
                  <div
                    key={vc.index}
                    className={`${styles.cell} ${
                      f.align === "right" ? styles.right : ""
                    } ${f.isNull ? styles.null : ""} ${
                      isSelected ? styles.selected : ""
                    } ${isSelected && isCopied ? styles.flash : ""}`}
                    style={{
                      left: colOffsets[vc.index],
                      width: leafColumns[vc.index].getSize(),
                    }}
                    title={f.text}
                    onClick={(e) => handleCellClick(vr.index, vc.index, e)}
                  >
                    {displayText}
                  </div>
                );
              })}
            </div>
          );
        })}
      </div>
    </div>
  );
}
