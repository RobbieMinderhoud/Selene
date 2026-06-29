/**
 * CSV import mapping menu.
 *
 * Driven by `useImportStore`: a schema-tree right-click sets a request, this
 * modal opens an OS file picker, analyses the CSV (header + sample + inferred
 * types), and renders a mapping menu — column types for a *new* table, or a
 * CSV→column mapping for an *existing* one. On import it streams progress into a
 * sticky toast (mirroring the export flow) and refreshes the affected tree node.
 *
 * Parse options (delimiter / quote / header) seed from Settings → Import and are
 * editable here; changing them re-analyses the file.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";

import {
  columnsList,
  importCsv,
  importCsvAnalyze,
  tableDrop,
} from "../ipc/commands";
import { createImportChannel } from "../ipc/channels";
import type {
  ColumnInfo,
  ColumnMappingArg,
  CsvAnalysis,
  ImportCsvOptions,
  ImportTargetArg,
} from "../ipc/types";
import { asIpcError } from "../ipc/types";
import { qk } from "../lib/queries";
import { useImportStore } from "../state/importStore";
import {
  useSettingsStore,
  type CsvDelimiter,
  type ImportQuoteChar,
} from "../state/settingsStore";
import { useToastStore } from "../state/toastStore";
import { Modal } from "./Modal";
import styles from "./CsvImportModal.module.css";

/** Curated SQL Server types offered for new-table columns. */
const BASE_TYPES = [
  "INT",
  "BIGINT",
  "DECIMAL(38,10)",
  "FLOAT",
  "BIT",
  "DATE",
  "DATETIME2",
  "NVARCHAR(255)",
  "NVARCHAR(MAX)",
  "UNIQUEIDENTIFIER",
];

/** Type dropdown options, always including the inferred default. */
function typeOptions(inferred: string): string[] {
  return BASE_TYPES.includes(inferred) ? BASE_TYPES : [inferred, ...BASE_TYPES];
}

function basename(path: string): string {
  return path.split(/[\\/]/).pop() ?? path;
}

/** Default new-table name from a file path: its base name without extension. */
function defaultTableName(path: string): string {
  const stem = basename(path)
    .replace(/\.[^.]+$/, "")
    .trim();
  return stem || "imported";
}

/** One new-table column, aligned to a CSV source field. */
interface NewColRow {
  csvIndex: number;
  name: string;
  sqlType: string;
  nullable: boolean;
  include: boolean;
}

/** The table an *import as new table* collided with — the drop+retry target. */
interface TableConflict {
  database: string | null;
  schema: string;
  table: string;
}

/**
 * Did an *import as new table* fail because the table already exists? SQL Server
 * raises error 2714 ("There is already an object named …") when CREATE TABLE
 * targets an existing name; the code is forwarded verbatim in the IPC message.
 */
function isTableExistsError(message: string): boolean {
  return (
    /\bcode 2714\b/.test(message) || /already an object named/i.test(message)
  );
}

export function CsvImportModal() {
  const request = useImportStore((s) => s.request);
  const closeImport = useImportStore((s) => s.closeImport);
  const queryClient = useQueryClient();

  const [path, setPath] = useState<string | null>(null);
  const [analysis, setAnalysis] = useState<CsvAnalysis | null>(null);
  const [existingCols, setExistingCols] = useState<ColumnInfo[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [importing, setImporting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Set when a new-table import collided with an existing table; drives the
  // two-step "drop & retry" recovery shown under the error.
  const [conflict, setConflict] = useState<TableConflict | null>(null);
  const [confirmDrop, setConfirmDrop] = useState(false);

  // Parse options (re-analyse on change); seeded from Settings → Import.
  const [delimiter, setDelimiter] = useState<string>(",");
  const [quoteChar, setQuoteChar] = useState<string>('"');
  const [hasHeader, setHasHeader] = useState(true);
  // Import-only options (don't affect parsing).
  const [emptyAsNull, setEmptyAsNull] = useState(true);
  const [atomic, setAtomic] = useState(true);

  // New-table form.
  const [tableName, setTableName] = useState("");
  const [newCols, setNewCols] = useState<NewColRow[]>([]);
  // Existing-table form: destination column name → CSV field index (or null).
  const [existingMap, setExistingMap] = useState<Record<string, number | null>>(
    {},
  );

  const open = request !== null;

  // Memoised so their identity is stable across re-renders (e.g. while typing
  // the table name) — an unstable `onClose` would otherwise re-fire Modal's
  // focus effect and steal focus from the input.
  const reset = useCallback(() => {
    setPath(null);
    setAnalysis(null);
    setExistingCols(null);
    setError(null);
    setConflict(null);
    setConfirmDrop(false);
    setBusy(false);
    setImporting(false);
    setTableName("");
    setNewCols([]);
    setExistingMap({});
  }, []);

  const close = useCallback(() => {
    if (importing) return; // don't drop a request mid-import
    reset();
    closeImport();
  }, [importing, reset, closeImport]);

  // A fresh request: seed options from settings, then open the file picker.
  const handledRef = useRef<typeof request>(null);
  useEffect(() => {
    if (!request) {
      handledRef.current = null;
      return;
    }
    if (handledRef.current === request) return;
    handledRef.current = request;

    const imp = useSettingsStore.getState().import;
    setDelimiter(imp.delimiter);
    setQuoteChar(imp.quoteChar);
    setHasHeader(imp.hasHeader);
    setEmptyAsNull(imp.emptyAsNull);
    setAtomic(imp.atomic);
    reset();

    void (async () => {
      const { open: openDialog } = await import("@tauri-apps/plugin-dialog");
      let picked: string | string[] | null;
      try {
        picked = await openDialog({
          title: "Import CSV",
          multiple: false,
          filters: [{ name: "CSV", extensions: ["csv", "tsv", "txt"] }],
        });
      } catch (e) {
        useToastStore.getState().push({
          kind: "error",
          message: "Import cancelled",
          detail: asIpcError(e).message,
        });
        closeImport();
        return;
      }
      if (typeof picked !== "string") {
        closeImport(); // user cancelled the file dialog
        return;
      }
      setPath(picked);
      // Seed the new-table name from the file name (the user can still edit it).
      if (request.mode === "new") setTableName(defaultTableName(picked));
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [request]);

  // (Re)analyse whenever the file or a parse option changes.
  useEffect(() => {
    if (!path || !request) return;
    let cancelled = false;
    setBusy(true);
    setError(null);

    void (async () => {
      try {
        const opts: ImportCsvOptions = {
          delimiter,
          quote: quoteChar,
          hasHeader,
        };
        const a = await importCsvAnalyze(path, opts);
        let cols: ColumnInfo[] | null = null;
        if (request.mode === "existing" && request.table) {
          cols = await columnsList(
            request.sessionId,
            request.database,
            request.schema,
            request.table,
          );
        }
        if (cancelled) return;
        setAnalysis(a);
        setExistingCols(cols);
        initForm(a, cols);
      } catch (e) {
        if (!cancelled) setError(asIpcError(e).message);
      } finally {
        if (!cancelled) setBusy(false);
      }
    })();

    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, delimiter, quoteChar, hasHeader]);

  /** Build the initial mapping form from a fresh analysis. */
  function initForm(a: CsvAnalysis, cols: ColumnInfo[] | null) {
    if (request?.mode === "new") {
      setNewCols(
        a.headers.map((h, i) => ({
          csvIndex: i,
          name: h.trim() || `column_${i + 1}`,
          sqlType: a.inferred[i]?.sql_type ?? "NVARCHAR(255)",
          nullable: true,
          include: true,
        })),
      );
    } else if (cols) {
      // Auto-match each table column to a CSV field by case-insensitive name.
      const map: Record<string, number | null> = {};
      for (const c of cols) {
        const idx = a.headers.findIndex(
          (h) => h.toLowerCase() === c.name.toLowerCase(),
        );
        map[c.name] = idx >= 0 ? idx : null;
      }
      setExistingMap(map);
    }
  }

  const includedNew = newCols.filter((c) => c.include);
  const mappedExistingCount = Object.values(existingMap).filter(
    (v) => v !== null,
  ).length;
  const canImport =
    !!analysis &&
    !importing &&
    (request?.mode === "new"
      ? tableName.trim().length > 0 && includedNew.length > 0
      : mappedExistingCount > 0);

  async function doImport() {
    if (!request || !path || !analysis) return;

    const options: ImportCsvOptions = {
      delimiter,
      quote: quoteChar,
      hasHeader,
      emptyAsNull,
      atomic,
    };

    let target: ImportTargetArg;
    let mapping: ColumnMappingArg[];
    let label: string;

    if (request.mode === "new") {
      const name = tableName.trim();
      const names = includedNew.map((c) => c.name.trim());
      if (new Set(names.map((n) => n.toLowerCase())).size !== names.length) {
        setError("Column names must be unique.");
        return;
      }
      if (names.some((n) => n.length === 0)) {
        setError("Every included column needs a name.");
        return;
      }
      target = {
        kind: "new",
        database: request.database,
        schema: request.schema,
        table: name,
        columns: includedNew.map((c) => ({
          name: c.name.trim(),
          sqlType: c.sqlType,
          nullable: c.nullable,
        })),
      };
      mapping = includedNew.map((c) => ({
        csvIndex: c.csvIndex,
        targetColumn: c.name.trim(),
      }));
      label = name;
    } else {
      target = {
        kind: "existing",
        database: request.database,
        schema: request.schema,
        table: request.table!,
      };
      mapping = Object.entries(existingMap)
        .filter(([, idx]) => idx !== null)
        .map(([targetColumn, idx]) => ({
          targetColumn,
          csvIndex: idx as number,
        }));
      label = request.table!;
    }

    const toasts = useToastStore.getState();
    const toastId = toasts.push({
      kind: "info",
      message: `Importing into ${label}…`,
      detail: "0 rows",
      sticky: true,
    });
    const channel = createImportChannel((e) => {
      if (e.kind === "progress") {
        toasts.update(toastId, { detail: `${e.rows} rows` });
      }
    });

    setImporting(true);
    setError(null);
    setConflict(null);
    setConfirmDrop(false);
    try {
      const summary = await importCsv(
        request.sessionId,
        path,
        target,
        mapping,
        options,
        channel,
      );
      const skipped =
        summary.rows_skipped > 0 ? ` (${summary.rows_skipped} skipped)` : "";
      toasts.update(toastId, {
        kind: "success",
        message: `Imported ${summary.rows_inserted} rows${skipped}`,
        detail: label,
        sticky: false,
      });
      setTimeout(() => toasts.requestDismiss(toastId), 5000);

      // Refresh the affected tree node so the new table / new rows show up.
      if (request.mode === "new") {
        void queryClient.invalidateQueries({
          queryKey: qk.tables(
            request.sessionId,
            request.database,
            request.schema,
          ),
        });
      } else {
        void queryClient.invalidateQueries({
          queryKey: qk.columns(
            request.sessionId,
            request.database,
            request.schema,
            request.table!,
          ),
        });
      }

      setImporting(false);
      reset();
      closeImport();
    } catch (e) {
      const ipc = asIpcError(e);
      toasts.update(toastId, {
        kind: "error",
        message: "Import failed",
        detail: ipc.message,
        sticky: false,
      });
      setTimeout(() => toasts.requestDismiss(toastId), 6000);
      setError(ipc.message);
      setImporting(false);
      // A new-table import that collided with an existing table can be retried
      // after dropping it — offer that as an explicit, confirmed recovery.
      if (target.kind === "new" && isTableExistsError(ipc.message)) {
        setConflict({
          database: target.database,
          schema: target.schema,
          table: target.table,
        });
      }
    }
  }

  /** Drop the collided table (after the two-step confirm) and re-run the import. */
  async function dropAndRetry() {
    if (!request || !conflict) return;
    setConfirmDrop(false);
    setImporting(true);
    setError(null);

    const toasts = useToastStore.getState();
    const toastId = toasts.push({
      kind: "info",
      message: `Dropping ${conflict.schema}.${conflict.table}…`,
      sticky: true,
    });
    try {
      await tableDrop(
        request.sessionId,
        conflict.database,
        conflict.schema,
        conflict.table,
      );
      toasts.requestDismiss(toastId);
      setConflict(null);
      setImporting(false);
      await doImport();
    } catch (e) {
      const ipc = asIpcError(e);
      toasts.update(toastId, {
        kind: "error",
        message: "Drop failed",
        detail: ipc.message,
        sticky: false,
      });
      setTimeout(() => toasts.requestDismiss(toastId), 6000);
      setError(ipc.message);
      setImporting(false);
    }
  }

  const title =
    request?.mode === "new"
      ? `Import CSV as new table in ${request.schema}`
      : `Import CSV into ${request?.schema}.${request?.table}`;

  const footer = (
    <>
      <button type="button" onClick={close} disabled={importing}>
        Cancel
      </button>
      <button
        type="button"
        className="primary"
        onClick={() => void doImport()}
        disabled={!canImport}
      >
        {importing ? "Importing…" : "Import"}
      </button>
    </>
  );

  return (
    <Modal
      open={open}
      title={title}
      onClose={close}
      footer={footer}
      width="min(920px, 94vw)"
    >
      <div className={styles.body}>
        {path && (
          <div className={styles.fileRow}>
            <span>File:</span>
            <span className={styles.fileName} title={path}>
              {basename(path)}
            </span>
          </div>
        )}

        {/* Parse options + (new) table name */}
        <div className={styles.controls}>
          {request?.mode === "new" && (
            <div className={`${styles.field} ${styles.grow}`}>
              <label htmlFor="csv-import-table">New table name</label>
              <input
                id="csv-import-table"
                value={tableName}
                placeholder="e.g. imported_policies"
                onChange={(e) => setTableName(e.target.value)}
              />
            </div>
          )}
          <div className={styles.field}>
            <label htmlFor="csv-import-delim">Delimiter</label>
            <select
              id="csv-import-delim"
              value={delimiter}
              onChange={(e) => {
                setDelimiter(e.target.value);
                // Remember the choice for next time.
                useSettingsStore
                  .getState()
                  .set("import", { delimiter: e.target.value as CsvDelimiter });
              }}
            >
              <option value=",">, (comma)</option>
              <option value=";">; (semicolon)</option>
              <option value={"\t"}>Tab</option>
              <option value="|">| (pipe)</option>
            </select>
          </div>
          <div className={styles.field}>
            <label htmlFor="csv-import-quote">Quote</label>
            <select
              id="csv-import-quote"
              value={quoteChar}
              onChange={(e) => {
                setQuoteChar(e.target.value);
                useSettingsStore.getState().set("import", {
                  quoteChar: e.target.value as ImportQuoteChar,
                });
              }}
            >
              <option value={'"'}>" (double)</option>
              <option value={"'"}>' (single)</option>
              <option value="none">No quotes</option>
            </select>
          </div>
        </div>

        {analysis && analysis.rawPreview.length > 0 && (
          <div>
            <p className={styles.sectionLabel}>
              File preview — check the delimiter matches above
            </p>
            <pre className={styles.previewBox}>
              {analysis.rawPreview.join("\n")}
            </pre>
          </div>
        )}

        <div className={styles.toggles}>
          <label className={styles.toggle}>
            <input
              type="checkbox"
              checked={hasHeader}
              onChange={(e) => setHasHeader(e.target.checked)}
            />
            First row is a header
          </label>
          <label className={styles.toggle}>
            <input
              type="checkbox"
              checked={emptyAsNull}
              onChange={(e) => setEmptyAsNull(e.target.checked)}
            />
            Treat empty as NULL
          </label>
          <label className={styles.toggle}>
            <input
              type="checkbox"
              checked={atomic}
              onChange={(e) => setAtomic(e.target.checked)}
            />
            Abort on first bad row
          </label>
        </div>

        {busy && (
          <div className={styles.loading}>
            <span className="spinner" aria-hidden /> Analysing CSV…
          </div>
        )}
        {error && (
          <div className={styles.error}>
            <span>{error}</span>
            {conflict && !importing && (
              <div className={styles.recover}>
                {confirmDrop ? (
                  <>
                    <button
                      type="button"
                      className="danger"
                      onClick={() => void dropAndRetry()}
                    >
                      Confirm: drop {conflict.schema}.{conflict.table}?
                    </button>
                    <button type="button" onClick={() => setConfirmDrop(false)}>
                      Cancel
                    </button>
                  </>
                ) : (
                  <button type="button" onClick={() => setConfirmDrop(true)}>
                    Drop table &amp; retry
                  </button>
                )}
              </div>
            )}
          </div>
        )}

        {analysis && !busy && request?.mode === "new" && (
          <>
            <p className={styles.sectionLabel}>
              Columns ({includedNew.length} of {newCols.length})
            </p>
            <div className={styles.mapList}>
              {newCols.map((col, i) => (
                <div
                  key={col.csvIndex}
                  className={`${styles.newRow} ${col.include ? "" : styles.disabled}`}
                >
                  <input
                    type="checkbox"
                    aria-label={`Include column ${analysis.headers[i]}`}
                    checked={col.include}
                    onChange={(e) =>
                      setNewCols((cs) =>
                        cs.map((c, j) =>
                          j === i ? { ...c, include: e.target.checked } : c,
                        ),
                      )
                    }
                  />
                  <span className={styles.csvName} title={analysis.headers[i]}>
                    {analysis.headers[i]}
                  </span>
                  <span className={styles.arrow} aria-hidden>
                    →
                  </span>
                  <input
                    aria-label={`Destination name for ${analysis.headers[i]}`}
                    value={col.name}
                    disabled={!col.include}
                    onChange={(e) =>
                      setNewCols((cs) =>
                        cs.map((c, j) =>
                          j === i ? { ...c, name: e.target.value } : c,
                        ),
                      )
                    }
                  />
                  <select
                    aria-label={`Type for ${analysis.headers[i]}`}
                    value={col.sqlType}
                    disabled={!col.include}
                    onChange={(e) =>
                      setNewCols((cs) =>
                        cs.map((c, j) =>
                          j === i ? { ...c, sqlType: e.target.value } : c,
                        ),
                      )
                    }
                  >
                    {typeOptions(col.sqlType).map((t) => (
                      <option key={t} value={t}>
                        {t}
                      </option>
                    ))}
                  </select>
                  <label
                    className={styles.toggle}
                    title="Allow NULL in this column"
                  >
                    <input
                      type="checkbox"
                      checked={col.nullable}
                      disabled={!col.include}
                      onChange={(e) =>
                        setNewCols((cs) =>
                          cs.map((c, j) =>
                            j === i ? { ...c, nullable: e.target.checked } : c,
                          ),
                        )
                      }
                    />
                    Null
                  </label>
                </div>
              ))}
            </div>
          </>
        )}

        {analysis && !busy && request?.mode === "existing" && existingCols && (
          <>
            <p className={styles.sectionLabel}>
              Map CSV fields to columns ({mappedExistingCount} mapped)
            </p>
            <div className={styles.mapList}>
              {existingCols.map((col) => {
                const sel = existingMap[col.name] ?? null;
                const unmappedNotNull = sel === null && !col.nullable;
                return (
                  <div key={col.name} className={styles.existRow}>
                    <span className={styles.destCol}>
                      <span className={styles.destName}>
                        {col.name}
                        {!col.nullable && (
                          <span className={styles.notNull}> · NOT NULL</span>
                        )}
                      </span>
                      <span className={styles.destType}>{col.data_type}</span>
                    </span>
                    <span className={styles.destCol}>
                      <select
                        aria-label={`CSV source for ${col.name}`}
                        value={sel === null ? "" : String(sel)}
                        onChange={(e) =>
                          setExistingMap((m) => ({
                            ...m,
                            [col.name]:
                              e.target.value === ""
                                ? null
                                : Number(e.target.value),
                          }))
                        }
                      >
                        <option value="">— skip —</option>
                        {analysis.headers.map((h, i) => (
                          <option key={i} value={String(i)}>
                            {h}
                          </option>
                        ))}
                      </select>
                      {unmappedNotNull && (
                        <span className={styles.warn}>
                          unmapped — needs a default or will error
                        </span>
                      )}
                    </span>
                  </div>
                );
              })}
            </div>
          </>
        )}
      </div>
    </Modal>
  );
}
