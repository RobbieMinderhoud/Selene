/**
 * Results panel: result-set sub-tabs + grid + status bar + export menu.
 *
 * Subscribes to the active tab's ResultState. Multi-statement batches produce
 * several result sets, shown as sub-tabs; DML sets (no columns) show their
 * affected-row count instead of a grid.
 */

import { useState } from "react";

import { exportResult } from "../ipc/commands";
import { createExportChannel } from "../ipc/channels";
import { asIpcError } from "../ipc/types";
import type { ExportFormat } from "../ipc/types";
import { isMac } from "../lib/platform";
import { useEditorStore } from "../state/editorStore";
import type { ResultState } from "../state/editorStore";
import { selectSession, useSessionStore } from "../state/sessionStore";
import { useSettingsStore } from "../state/settingsStore";
import type { CsvOptions } from "../ipc/types";
import { toastError, useToastStore } from "../state/toastStore";
import { DropdownIcon } from "./icons";
import { ResultsGrid } from "./ResultsGrid";
import styles from "./ResultsPanel.module.css";

interface ResultsPanelProps {
  tabId: string;
}

const EXPORT_FORMATS: { format: ExportFormat; label: string; ext: string }[] = [
  { format: "csv", label: "CSV", ext: "csv" },
  { format: "json", label: "JSON", ext: "json" },
  { format: "xlsx", label: "Excel (.xlsx)", ext: "xlsx" },
];

export function ResultsPanel({ tabId }: ResultsPanelProps) {
  const result = useEditorStore((s) => s.results[tabId]) as
    | ResultState
    | undefined;
  const setActiveSet = useEditorStore((s) => s.setActiveSet);
  const tab = useEditorStore((s) => s.tabs.find((t) => t.id === tabId));
  const session = useSessionStore((s) =>
    selectSession(s, tab?.sessionId ?? null),
  );

  const exportSettings = useSettingsStore((s) => s.export);
  const [exportOpen, setExportOpen] = useState(false);

  if (!result || result.status === "idle") {
    return (
      <div className={styles.panel}>
        <div className={styles.placeholder}>
          Run a query to see results. Press <kbd>{isMac ? "⌘" : "Ctrl"}</kbd>
          <kbd>Enter</kbd> to run.
        </div>
      </div>
    );
  }

  const active = result.resultSets[result.activeSet];

  async function doExport(format: ExportFormat) {
    setExportOpen(false);
    if (!tab?.sessionId || !tab.sql.trim()) {
      toastError("Nothing to export", "Connect and run a query first.");
      return;
    }
    const fmt = EXPORT_FORMATS.find((f) => f.format === format)!;
    const { save } = await import("@tauri-apps/plugin-dialog");

    const now = new Date();
    const dateTime = now.toISOString().replace(/[:.]/g, "-").slice(0, -5);
    const fileName = `Selene-${tab.currentDatabase || "query"}-${dateTime}.${fmt.ext}`;

    let path: string | null;
    try {
      path = await save({
        title: `Export result as ${fmt.label}`,
        defaultPath: fileName,
        filters: [{ name: fmt.label, extensions: [fmt.ext] }],
      });
    } catch (e) {
      toastError("Export cancelled", asIpcError(e).message);
      return;
    }
    if (!path) return; // user cancelled the dialog

    const toasts = useToastStore.getState();
    const toastId = toasts.push({
      kind: "info",
      message: `Exporting to ${fmt.label}…`,
      detail: "0 rows",
      sticky: true,
    });

    const channel = createExportChannel((event) => {
      if (event.kind === "progress") {
        toasts.update(toastId, { detail: `${event.rows} rows` });
      }
    });

    try {
      const csvOptions: CsvOptions | undefined =
        format === "csv"
          ? {
              delimiter: exportSettings.delimiter,
              quote: exportSettings.quoteChar,
              quote_style: exportSettings.quoteStyle,
              line_ending: exportSettings.lineEnding,
              include_header: exportSettings.includeHeader,
              bom: exportSettings.bom,
            }
          : undefined;
      const summary = await exportResult(
        tab.sessionId,
        tab.sql,
        format,
        path,
        undefined,
        channel,
        csvOptions,
      );
      toasts.update(toastId, {
        kind: "success",
        message: `Exported ${summary.rows_written} rows`,
        detail: path,
        sticky: false,
      });
      // Auto-dismiss the now-success toast (animated out like the rest).
      setTimeout(() => toasts.requestDismiss(toastId), 5000);
    } catch (e) {
      const ipc = asIpcError(e);
      toasts.update(toastId, {
        kind: "error",
        message: "Export failed",
        detail: ipc.message,
        sticky: false,
      });
      setTimeout(() => toasts.requestDismiss(toastId), 6000);
    }
  }

  return (
    <div className={styles.panel}>
      <div className={styles.toolbar}>
        <div className={styles.setTabs} role="tablist" aria-label="Result sets">
          {result.resultSets.length === 0 && result.status === "running" && (
            <span className={styles.runningLabel}>
              <span className="spinner" aria-hidden /> Running…
            </span>
          )}
          {result.resultSets.map((rs, i) => (
            <button
              key={rs.setIndex}
              type="button"
              role="tab"
              aria-selected={i === result.activeSet}
              className={`${styles.setTab} ${
                i === result.activeSet ? styles.setTabActive : ""
              }`}
              onClick={() => setActiveSet(tabId, i)}
            >
              {rs.columns.length > 0 ? `Result ${i + 1}` : `Set ${i + 1}`}
            </button>
          ))}
        </div>

        <div className={styles.exportWrap}>
          <button
            type="button"
            disabled={!active || active.columns.length === 0}
            onClick={() => setExportOpen((o) => !o)}
            aria-haspopup="menu"
            aria-expanded={exportOpen}
          >
            Export
            <DropdownIcon />
          </button>
          {exportOpen && (
            <>
              <div
                className={styles.menuBackdrop}
                onClick={() => setExportOpen(false)}
              />
              <div className={styles.menu} role="menu">
                {EXPORT_FORMATS.map((f) => (
                  <button
                    key={f.format}
                    type="button"
                    role="menuitem"
                    className={styles.menuItem}
                    onClick={() => doExport(f.format)}
                  >
                    {f.label}
                  </button>
                ))}
              </div>
            </>
          )}
        </div>
      </div>

      <div className={styles.body}>
        {result.status === "failed" ? (
          <div className={styles.errorBox}>
            <strong>Query failed</strong>
            <pre>{result.error}</pre>
          </div>
        ) : !active ? (
          <div className={styles.placeholder}>
            {result.status === "running"
              ? "Waiting for the first result set…"
              : "No result set."}
          </div>
        ) : active.columns.length === 0 ? (
          <div className={styles.placeholder}>
            {active.affected != null
              ? `${result.rolledBack ? "Rolled back · " : ""}${active.affected} row${active.affected === 1 ? "" : "s"} affected.`
              : "Statement executed (no result set)."}
          </div>
        ) : (
          // Key by runId + set index so a new query gets a fresh virtualizer
          // (scroll position / measurement cache reset), while appends within
          // one run reuse the instance and stay smooth. runId is stable for the
          // whole run (unlike queryId, which is nulled on finish).
          <ResultsGrid
            key={`${result.runId}:${active.setIndex}`}
            resultSet={active}
            rev={result.rev}
          />
        )}
      </div>

      <StatusBar
        status={result.status}
        rowCount={
          active && active.columns.length > 0
            ? active.rows.length
            : result.rowCount
        }
        elapsedMs={result.elapsedMs}
        truncated={result.truncated}
        readOnly={session?.readOnly ?? false}
      />
    </div>
  );
}

function StatusBar({
  status,
  rowCount,
  elapsedMs,
  truncated,
  readOnly,
}: {
  status: ResultState["status"];
  rowCount: number;
  elapsedMs: number | null;
  truncated: boolean;
  readOnly: boolean;
}) {
  return (
    <div className={styles.statusBar}>
      <span className={`${styles.statusDot} ${styles[status]}`} aria-hidden />
      <span className={styles.statusText}>{statusLabel(status)}</span>
      <span className={styles.sep}>·</span>
      <span>
        {rowCount.toLocaleString()} row{rowCount === 1 ? "" : "s"}
      </span>
      {elapsedMs != null && (
        <>
          <span className={styles.sep}>·</span>
          <span>{elapsedMs.toLocaleString()} ms</span>
        </>
      )}
      {truncated && (
        <>
          <span className={styles.sep}>·</span>
          <span className={styles.truncated} title="Row limit reached">
            truncated
          </span>
        </>
      )}
      {readOnly && <span className={styles.roTag}>read-only</span>}
    </div>
  );
}

function statusLabel(status: ResultState["status"]): string {
  switch (status) {
    case "running":
      return "Running";
    case "done":
      return "Done";
    case "cancelled":
      return "Cancelled";
    case "failed":
      return "Failed";
    default:
      return "Idle";
  }
}
