/**
 * "Run on multiple targets" — a dedicated main-area view (its own tab kind).
 *
 * Pick servers (saved connections), choose the databases per server (by a filter
 * query previewed live, or hand-picked from a list), then either generate a
 * copy-pasteable per-database script, execute the query across every target, or
 * fetch the combined results into the grid (and Save CSV).
 *
 * The form + last run's progress live in `multiTargetStore` (keyed by this
 * tab id) so they survive tab switches. The aggregated results stream into this
 * tab's `editorStore` result slot, so the existing `ResultsGrid` renders them.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import CodeMirror from "@uiw/react-codemirror";
import { sql, MSSQL } from "@codemirror/lang-sql";
import { EditorView } from "@codemirror/view";
import { githubDark, githubLight } from "@uiw/codemirror-theme-github";

import { createMultiChannel } from "../ipc/channels";
import {
  exportResultSet,
  guardCheck,
  multiTargetCancel,
  multiTargetResolve,
  multiTargetRun,
} from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import type {
  CsvOptions,
  GuardVerdict,
  MultiEvent,
  MultiMode,
} from "../ipc/types";
import { buildScriptText } from "../lib/multiTargetScript";
import { useConnections } from "../lib/queries";
import { useEditorStore } from "../state/editorStore";
import {
  useMultiTargetStore,
  useMultiTargetView,
} from "../state/multiTargetStore";
import { useSettingsStore } from "../state/settingsStore";
import { useThemeStore } from "../state/themeStore";
import { toastError, toastInfo, useToastStore } from "../state/toastStore";
import { GuardModal } from "./GuardModal";
import {
  CancelIcon,
  CheckIcon,
  CopyIcon,
  DownloadIcon,
  ErrorIcon,
  MultiTargetIcon,
  RunIcon,
} from "./icons";
import { ResultsGrid } from "./ResultsGrid";
import styles from "./MultiTargetPane.module.css";

/** List-mode database source: every database, so the user can hand-pick. */
const LIST_ALL_DATABASES_SQL = "SELECT name FROM sys.databases ORDER BY name";

interface MultiTargetPaneProps {
  tabId: string;
}

type GuardPrompt =
  | { kind: "confirm"; verdict: GuardVerdict }
  | { kind: "block"; verdict: GuardVerdict }
  | null;

/** Filterable outcome buckets for the progress list. */
type ProgressCategory = "ok" | "warning" | "error";

/**
 * Classify one progress row, or `null` while it is still pending. A "warning"
 * is an execute-mode database whose DML affected 0 rows (ran, changed nothing).
 */
function categoryOf(
  p: { status: string; rows: number | null },
  execute: boolean,
): ProgressCategory | null {
  if (p.status === "error") return "error";
  if (p.status === "ok") return execute && p.rows === 0 ? "warning" : "ok";
  return null; // pending
}

/** A timestamp suitable for a default file name. */
function fileStamp(): string {
  return new Date().toISOString().replace(/[:.]/g, "-").slice(0, -5);
}

export function MultiTargetPane({ tabId }: MultiTargetPaneProps) {
  const view = useMultiTargetView(tabId);
  const ensure = useMultiTargetStore((s) => s.ensure);
  const update = useMultiTargetStore((s) => s.update);
  const toggleConnection = useMultiTargetStore((s) => s.toggleConnection);
  const setListSelection = useMultiTargetStore((s) => s.setListSelection);

  const { data: connections = [] } = useConnections();

  // Create the view once, seeding the filter query from settings.
  useEffect(() => {
    ensure(tabId, useSettingsStore.getState().multiTarget.defaultFilterQuery);
  }, [tabId, ensure]);

  const [guard, setGuard] = useState<GuardPrompt>(null);
  const confirmResolver = useRef<((ok: boolean) => void) | null>(null);
  const runInFlight = useRef(false);

  // Auto-follow the progress list as targets complete, unless the user has
  // scrolled up to read earlier rows (then leave their position alone).
  const progressRef = useRef<HTMLDivElement | null>(null);
  const stickToBottom = useRef(true);
  const progressCount = view?.progress.length ?? 0;
  useEffect(() => {
    const el = progressRef.current;
    if (el && stickToBottom.current) el.scrollTop = el.scrollHeight;
  }, [progressCount]);

  // Status filter for the progress list (empty set = show everything).
  const [progressFilters, setProgressFilters] = useState<Set<ProgressCategory>>(
    () => new Set(),
  );
  function toggleProgressFilter(cat: ProgressCategory) {
    setProgressFilters((prev) => {
      const next = new Set(prev);
      if (next.has(cat)) next.delete(cat);
      else next.add(cat);
      return next;
    });
  }

  const connName = useCallback(
    (id: string) => connections.find((c) => c.id === id)?.name ?? id,
    [connections],
  );

  // Stable editor change handlers (CodeMirror reconfigures when onChange changes).
  const onFilterChange = useCallback(
    (v: string) => update(tabId, { filterSql: v }),
    [update, tabId],
  );
  const onQueryChange = useCallback(
    (v: string) => update(tabId, { querySql: v }),
    [update, tabId],
  );

  if (!view) {
    return <div className={styles.loading}>Loading…</div>;
  }

  const selected = view.selectedConnectionIds;
  const isRunning = view.runStatus === "running";
  const allServersSelected =
    connections.length > 0 && selected.length === connections.length;

  /** The explicit (server, databases) plan from the current selection. */
  function buildTargets(): { connectionId: string; databases: string[] }[] {
    if (!view) return [];
    if (view.dbMode === "query") {
      const sel = new Set(view.selectedConnectionIds);
      return view.resolved
        .filter(
          (r) => sel.has(r.connectionId) && !r.error && r.databases.length,
        )
        .map((r) => ({ connectionId: r.connectionId, databases: r.databases }));
    }
    return view.selectedConnectionIds
      .map((id) => ({
        connectionId: id,
        databases: view.listSelections[id] ?? [],
      }))
      .filter((t) => t.databases.length > 0);
  }

  const targetCount = buildTargets().reduce(
    (n, t) => n + t.databases.length,
    0,
  );

  // ── Preview / load databases ──────────────────────────────────────────────
  async function preview() {
    if (!view || !selected.length) {
      toastError("No servers selected", "Pick at least one server first.");
      return;
    }
    update(tabId, { resolving: true, error: null });
    try {
      const filter =
        view.dbMode === "query" ? view.filterSql : LIST_ALL_DATABASES_SQL;
      const resolved = await multiTargetResolve(selected, filter);
      update(tabId, { resolved, resolving: false });
      // List mode: default every matched database to selected, ready to prune.
      if (view.dbMode === "list") {
        for (const r of resolved) {
          if (!r.error) setListSelection(tabId, r.connectionId, r.databases);
        }
      }
    } catch (e) {
      update(tabId, { resolving: false, error: asIpcError(e).message });
    }
  }

  // ── Generate script ────────────────────────────────────────────────────────
  function generateScript() {
    if (!view) return;
    const targets = buildTargets();
    if (!targets.length) {
      toastError("No targets", "Select servers and databases first.");
      return;
    }
    if (!view.querySql.trim()) {
      toastError("No query", "Enter a query to run.");
      return;
    }
    const ed = useEditorStore.getState();
    // One script per server (`USE` can't cross servers).
    for (const t of targets) {
      const script = buildScriptText(t.databases, view.querySql);
      const id = ed.addTab(null, script);
      ed.renameTab(id, `Script · ${connName(t.connectionId)}`);
    }
    toastInfo(
      targets.length === 1
        ? "Script opened in a new tab"
        : `${targets.length} scripts opened in new tabs`,
    );
  }

  // ── Run (execute / results) ─────────────────────────────────────────────────
  function handleEvent(ev: MultiEvent, mode: MultiMode) {
    const store = useMultiTargetStore.getState();
    const ed = useEditorStore.getState();
    switch (ev.kind) {
      case "started":
        store.update(tabId, { runId: ev.runId, total: ev.total });
        if (mode === "results") ed.resultStarted(tabId, ev.runId);
        break;
      case "target":
        store.markPending(tabId, ev.connectionId, ev.server, ev.database);
        break;
      case "meta":
        ed.resultMeta(tabId, 0, ev.columns);
        break;
      case "rows":
        ed.resultAppendRows(tabId, 0, ev.rows);
        break;
      case "targetDone":
        store.markDone(
          tabId,
          ev.connectionId,
          ev.server,
          ev.database,
          ev.rows,
          ev.error,
        );
        break;
      case "serverError":
        store.markServerError(tabId, ev.connectionId, ev.server, ev.error);
        break;
      case "finished":
        store.finishRun(tabId, ev.succeeded, ev.failed, ev.rowsTotal);
        if (mode === "results")
          ed.resultFinished(
            tabId,
            {
              result_sets: 1,
              total_rows: ev.rowsTotal,
              truncated: false,
              rolled_back: false,
            },
            0,
          );
        break;
      case "cancelled":
        store.cancelRun(tabId);
        if (mode === "results") ed.resultCancelled(tabId);
        break;
    }
  }

  async function startRun(mode: MultiMode) {
    if (!view || runInFlight.current || isRunning) return;
    const targets = buildTargets();
    if (!targets.length) {
      toastError("No targets", "Select servers and databases first.");
      return;
    }
    if (!view.querySql.trim()) {
      toastError("No query", "Enter a query to run.");
      return;
    }

    runInFlight.current = true;
    try {
      // Guard once. `results` is read-only by nature, so a non-SELECT blocks;
      // `execute` is read-write, so destructive statements prompt to confirm.
      // The backend additionally enforces each connection's read-only flag.
      const verdict = await guardCheck(view.querySql, mode === "results");
      if (verdict.level === "block") {
        setGuard({ kind: "block", verdict });
        return;
      }
      if (verdict.level === "confirm") {
        const ok = await new Promise<boolean>((resolve) => {
          confirmResolver.current = resolve;
          setGuard({ kind: "confirm", verdict });
        });
        if (!ok) return;
      }

      const settings = useSettingsStore.getState();
      const total = targets.reduce((n, t) => n + t.databases.length, 0);

      // Prime the stores before the first event lands. Re-follow the progress
      // list from the top of this run regardless of where it was left.
      stickToBottom.current = true;
      useMultiTargetStore.getState().startRun(tabId, mode, total, "");
      if (mode === "results") useEditorStore.getState().resetResult(tabId);

      const channel = createMultiChannel((ev) => handleEvent(ev, mode));
      try {
        const { runId } = await multiTargetRun(
          targets,
          view.querySql,
          mode,
          settings.results.defaultRowLimit,
          settings.multiTarget.maxParallelServers,
          channel,
        );
        // Keep the cancel handle even if the `started` event raced ahead.
        useMultiTargetStore.getState().update(tabId, { runId });
      } catch (e) {
        const ipc = asIpcError(e);
        useMultiTargetStore.getState().update(tabId, { runStatus: "done" });
        toastError("Run failed", ipc.message);
      }
    } finally {
      runInFlight.current = false;
    }
  }

  function resolveConfirm(ok: boolean) {
    setGuard(null);
    confirmResolver.current?.(ok);
    confirmResolver.current = null;
  }

  function stop() {
    if (view?.runId) void multiTargetCancel(view.runId).catch(() => undefined);
  }

  // ── Derived progress numbers ────────────────────────────────────────────────
  const execute = view.runMode === "execute";
  let okCount = 0;
  let warnCount = 0;
  let errCount = 0;
  for (const p of view.progress) {
    const cat = categoryOf(p, execute);
    if (cat === "ok") okCount += 1;
    else if (cat === "warning") warnCount += 1;
    else if (cat === "error") errCount += 1;
  }
  const completed = view.progress.filter(
    (p) => p.database !== "" && p.status !== "pending",
  ).length;
  const pct =
    view.runStatus === "done"
      ? 100
      : view.total
        ? Math.min(100, Math.round((completed / view.total) * 100))
        : 0;

  // Apply the status filter (empty = show all). Pending rows only show under
  // the unfiltered view, so filtering to "errors" surfaces just the problems.
  const visibleProgress =
    progressFilters.size === 0
      ? view.progress
      : view.progress.filter((p) => {
          const cat = categoryOf(p, execute);
          return cat !== null && progressFilters.has(cat);
        });

  return (
    <div className={styles.pane}>
      <div className={styles.scroll}>
        {/* Servers */}
        <section className={styles.section}>
          <header className={styles.head}>
            <MultiTargetIcon />
            <h2>Servers</h2>
            <div className={styles.headRight}>
              <span className={styles.muted}>{selected.length} selected</span>
              {connections.length > 0 && (
                <button
                  type="button"
                  className={styles.linkBtn}
                  onClick={() =>
                    update(tabId, {
                      selectedConnectionIds: allServersSelected
                        ? []
                        : connections.map((c) => c.id),
                    })
                  }
                >
                  {allServersSelected ? "Clear" : "Select all"}
                </button>
              )}
            </div>
          </header>
          {connections.length === 0 ? (
            <p className={styles.muted}>
              No saved connections. Add one in the sidebar first.
            </p>
          ) : (
            <div className={styles.list}>
              {connections.map((c) => {
                const on = selected.includes(c.id);
                return (
                  <label
                    key={c.id}
                    className={`${styles.row} ${on ? styles.rowSelected : ""}`}
                  >
                    <input
                      type="checkbox"
                      checked={on}
                      onChange={() => toggleConnection(tabId, c.id)}
                    />
                    <span className={styles.rowName}>{c.name}</span>
                    {c.read_only && <span className={styles.roBadge}>RO</span>}
                    <span className={styles.rowMeta}>
                      {c.host}
                      {c.port ? `:${c.port}` : ""}
                    </span>
                  </label>
                );
              })}
            </div>
          )}
        </section>

        {/* Database selection */}
        <section className={styles.section}>
          <header className={styles.head}>
            <h2>Databases</h2>
            <div
              className={styles.segmented}
              role="tablist"
              aria-label="Database selection mode"
            >
              <button
                type="button"
                role="tab"
                aria-selected={view.dbMode === "query"}
                className={view.dbMode === "query" ? styles.segOn : ""}
                onClick={() => update(tabId, { dbMode: "query" })}
              >
                Filter query
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={view.dbMode === "list"}
                className={view.dbMode === "list" ? styles.segOn : ""}
                onClick={() => update(tabId, { dbMode: "list" })}
              >
                Pick from list
              </button>
            </div>
            <button
              type="button"
              onClick={() => void preview()}
              disabled={view.resolving || !selected.length}
            >
              {view.resolving
                ? "Loading…"
                : view.dbMode === "query"
                  ? "Preview"
                  : "Load databases"}
            </button>
          </header>

          {view.dbMode === "query" && (
            <MiniSqlEditor
              value={view.filterSql}
              onChange={onFilterChange}
              minHeight="84px"
              ariaLabel="Database filter query"
            />
          )}

          {view.error && <p className={styles.error}>{view.error}</p>}

          {/* Resolved preview / pick list */}
          {view.resolved.length > 0 && (
            <div className={styles.resolved}>
              {view.dbMode === "query" && (
                <p className={styles.muted}>
                  {targetCount} database{targetCount === 1 ? "" : "s"} across{" "}
                  {
                    view.resolved.filter(
                      (r) => selected.includes(r.connectionId) && !r.error,
                    ).length
                  }{" "}
                  server{selected.length === 1 ? "" : "s"}
                </p>
              )}
              {view.resolved
                .filter((r) => selected.includes(r.connectionId))
                .map((r) => {
                  const picked = view.listSelections[r.connectionId] ?? [];
                  const allPicked =
                    r.databases.length > 0 &&
                    picked.length === r.databases.length;
                  return (
                    <div key={r.connectionId} className={styles.resolvedServer}>
                      <div className={styles.resolvedHead}>
                        <strong>{r.server}</strong>
                        {r.error ? (
                          <span className={styles.error}>{r.error}</span>
                        ) : view.dbMode === "list" ? (
                          <>
                            <span className={styles.muted}>
                              {picked.length}/{r.databases.length} selected
                            </span>
                            <button
                              type="button"
                              className={`${styles.linkBtn} ${styles.pushRight}`}
                              onClick={() =>
                                setListSelection(
                                  tabId,
                                  r.connectionId,
                                  allPicked ? [] : r.databases,
                                )
                              }
                            >
                              {allPicked ? "Clear" : "Select all"}
                            </button>
                          </>
                        ) : (
                          <span className={styles.muted}>
                            {r.databases.length} database
                            {r.databases.length === 1 ? "" : "s"}
                          </span>
                        )}
                      </div>
                      {!r.error && view.dbMode === "list" && (
                        <div className={`${styles.list} ${styles.dbListBox}`}>
                          {r.databases.map((db) => {
                            const on = picked.includes(db);
                            return (
                              <label
                                key={db}
                                className={`${styles.row} ${on ? styles.rowSelected : ""}`}
                              >
                                <input
                                  type="checkbox"
                                  checked={on}
                                  onChange={(e) =>
                                    setListSelection(
                                      tabId,
                                      r.connectionId,
                                      e.target.checked
                                        ? [...picked, db]
                                        : picked.filter((d) => d !== db),
                                    )
                                  }
                                />
                                <span className={styles.rowName}>{db}</span>
                              </label>
                            );
                          })}
                        </div>
                      )}
                      {!r.error && view.dbMode === "query" && (
                        <div className={styles.dbNames}>
                          {r.databases.join(", ") || "—"}
                        </div>
                      )}
                    </div>
                  );
                })}
            </div>
          )}
        </section>

        {/* Query */}
        <section className={styles.section}>
          <header className={styles.head}>
            <h2>Query</h2>
            <span className={styles.muted}>
              runs against each of {targetCount} database
              {targetCount === 1 ? "" : "s"}
            </span>
          </header>
          <MiniSqlEditor
            value={view.querySql}
            onChange={onQueryChange}
            height="180px"
            ariaLabel="Query to run on each database"
          />
          <div className={styles.actions}>
            <button
              type="button"
              onClick={generateScript}
              disabled={isRunning || !targetCount}
              title="Build a per-database script and open it in a new tab"
            >
              <CopyIcon />
              Generate script
            </button>
            <button
              type="button"
              className="primary"
              onClick={() => void startRun("execute")}
              disabled={isRunning || !targetCount}
              title="Run the query on every selected database"
            >
              <RunIcon />
              Execute
            </button>
            <button
              type="button"
              className="primary"
              onClick={() => void startRun("results")}
              disabled={isRunning || !targetCount}
              title="Run a SELECT on every database and aggregate the rows"
            >
              <RunIcon />
              Fetch results
            </button>
            {isRunning && (
              <button type="button" className="danger" onClick={stop}>
                <CancelIcon />
                Stop
              </button>
            )}
          </div>
        </section>

        {/* Progress */}
        {view.runStatus !== "idle" && (
          <section className={styles.section}>
            <header className={styles.head}>
              <h2>Progress</h2>
              <span className={styles.muted}>
                {completed}/{view.total}
                {view.runStatus === "cancelled" ? " · stopped" : ""}
              </span>
            </header>
            <div className={styles.progressBar}>
              <div
                className={`${styles.progressFill} ${
                  errCount > 0 ? styles.progressFillWarn : ""
                }`}
                style={{ width: `${pct}%` }}
              />
            </div>
            {/* Filter the list to a status, to root out only the problems. */}
            <div className={styles.filterPills}>
              <button
                type="button"
                className={`${styles.pill} ${
                  progressFilters.size === 0 ? styles.pillOn : ""
                }`}
                onClick={() => setProgressFilters(new Set())}
              >
                All {view.progress.length}
              </button>
              <button
                type="button"
                className={`${styles.pill} ${
                  progressFilters.has("ok") ? styles.pillOn : ""
                }`}
                onClick={() => toggleProgressFilter("ok")}
                disabled={okCount === 0}
              >
                <span className={`${styles.pillDot} ${styles.pillDotOk}`} />
                OK {okCount}
              </button>
              {execute && (
                <button
                  type="button"
                  className={`${styles.pill} ${
                    progressFilters.has("warning") ? styles.pillOn : ""
                  }`}
                  onClick={() => toggleProgressFilter("warning")}
                  disabled={warnCount === 0}
                  title="Ran successfully but affected 0 rows"
                >
                  <span className={`${styles.pillDot} ${styles.pillDotWarn}`} />
                  0 affected {warnCount}
                </button>
              )}
              <button
                type="button"
                className={`${styles.pill} ${
                  progressFilters.has("error") ? styles.pillOn : ""
                }`}
                onClick={() => toggleProgressFilter("error")}
                disabled={errCount === 0}
              >
                <span className={`${styles.pillDot} ${styles.pillDotError}`} />
                Failed {errCount}
              </button>
            </div>
            <div
              className={styles.targetList}
              ref={progressRef}
              onScroll={() => {
                const el = progressRef.current;
                if (el) {
                  // Keep following only while pinned near the bottom (~24px).
                  stickToBottom.current =
                    el.scrollHeight - el.scrollTop - el.clientHeight < 24;
                }
              }}
            >
              {visibleProgress.length === 0 && (
                <div className={styles.placeholderRow}>
                  No matching targets.
                </div>
              )}
              {visibleProgress.map((p) => {
                // Flag a database where the executed DML changed nothing.
                const zeroAffected =
                  execute && p.status === "ok" && p.rows === 0;
                return (
                  <div
                    key={`${p.connectionId} ${p.database}`}
                    className={styles.targetRow}
                    data-status={p.status}
                    data-zero={zeroAffected ? "true" : undefined}
                  >
                    <span className={styles.targetIcon}>
                      {p.status === "ok" ? (
                        <CheckIcon />
                      ) : p.status === "error" ? (
                        <ErrorIcon />
                      ) : (
                        <span className="spinner" aria-hidden />
                      )}
                    </span>
                    <span className={styles.targetName}>
                      {p.server}
                      {p.database ? ` › ${p.database}` : " › (server)"}
                    </span>
                    {p.rows != null && (
                      <span className={styles.muted}>
                        {p.rows} {execute ? "affected" : "rows"}
                      </span>
                    )}
                    {p.error && (
                      <span className={styles.targetError}>{p.error}</span>
                    )}
                  </div>
                );
              })}
            </div>
          </section>
        )}

        {/* Aggregated results (results mode). Isolated child so streamed row
            batches re-render only the grid — never the editors above. */}
        {view.runMode === "results" && view.runStatus !== "idle" && (
          <MultiResults tabId={tabId} isRunning={isRunning} />
        )}
      </div>

      <GuardModal
        state={guard}
        onConfirm={() => resolveConfirm(true)}
        onCancel={() => resolveConfirm(false)}
      />
    </div>
  );
}

/**
 * The aggregated results grid + Save CSV, isolated from the form. It is the
 * only thing that subscribes to the editor-store result slot, so the frequent
 * streamed-row appends (which bump the slot's `rev`) re-render the grid alone —
 * the servers/databases/query editors above never repaint mid-run.
 */
function MultiResults({
  tabId,
  isRunning,
}: {
  tabId: string;
  isRunning: boolean;
}) {
  const result = useEditorStore((s) => s.results[tabId]);
  const resultSet = result?.resultSets[0];
  const canSaveCsv = !!resultSet && resultSet.columns.length > 0 && !isRunning;

  async function saveCsv() {
    const set = useEditorStore.getState().results[tabId]?.resultSets[0];
    if (!set || set.columns.length === 0) {
      toastError("Nothing to save", "Fetch results first.");
      return;
    }
    const { save } = await import("@tauri-apps/plugin-dialog");
    let path: string | null;
    try {
      path = await save({
        title: "Save results as CSV",
        defaultPath: `Selene-multi-${fileStamp()}.csv`,
        filters: [{ name: "CSV", extensions: ["csv"] }],
      });
    } catch (e) {
      toastError("Save cancelled", asIpcError(e).message);
      return;
    }
    if (!path) return;

    const s = useSettingsStore.getState();
    const csvOptions: CsvOptions = {
      delimiter: s.multiTarget.csvDelimiter,
      quote: s.export.quoteChar,
      quote_style: s.export.quoteStyle,
      line_ending: s.export.lineEnding,
      include_header: s.export.includeHeader,
      bom: s.multiTarget.csvBom,
    };
    const toasts = useToastStore.getState();
    const toastId = toasts.push({
      kind: "info",
      message: "Saving CSV…",
      sticky: true,
    });
    try {
      const summary = await exportResultSet(
        set.columns,
        set.rows,
        "csv",
        path,
        csvOptions,
      );
      toasts.update(toastId, {
        kind: "success",
        message: `Saved ${summary.rows_written} rows`,
        detail: path,
        sticky: false,
      });
      setTimeout(() => toasts.requestDismiss(toastId), 5000);
    } catch (e) {
      toasts.update(toastId, {
        kind: "error",
        message: "Save failed",
        detail: asIpcError(e).message,
        sticky: false,
      });
      setTimeout(() => toasts.requestDismiss(toastId), 6000);
    }
  }

  return (
    <section className={`${styles.section} ${styles.resultsSection}`}>
      <header className={styles.head}>
        <h2>Results</h2>
        <button
          type="button"
          onClick={() => void saveCsv()}
          disabled={!canSaveCsv}
          title="Save the combined results as CSV"
        >
          <DownloadIcon />
          Save CSV
        </button>
      </header>
      <div className={styles.grid}>
        {resultSet && resultSet.columns.length > 0 ? (
          <ResultsGrid
            key={`${result?.runId}:0`}
            resultSet={resultSet}
            rev={result?.rev ?? 0}
          />
        ) : (
          <div className={styles.placeholder}>
            {isRunning ? "Waiting for rows…" : "No rows returned."}
          </div>
        )}
      </div>
    </section>
  );
}

/**
 * A small controlled CodeMirror (MSSQL dialect) for the filter + query editors.
 * Unlike the main `SqlEditor` it is value/onChange-driven (not bound to an
 * editor-store tab) and skips autocomplete/search — it's a focused input.
 */
function MiniSqlEditor({
  value,
  onChange,
  height,
  minHeight,
  ariaLabel,
}: {
  value: string;
  onChange: (v: string) => void;
  height?: string;
  minHeight?: string;
  ariaLabel: string;
}) {
  const themeMode = useThemeStore((s) => s.mode);
  const fontSize = useSettingsStore((s) => s.editor.fontSize);
  const extensions = useMemo(
    () => [
      sql({ dialect: MSSQL }),
      EditorView.lineWrapping,
      EditorView.theme({
        "&": { fontSize: `${fontSize}px` },
        ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "1.5" },
        ".cm-content": { paddingBlock: "6px" },
      }),
    ],
    [fontSize],
  );
  return (
    <div className={styles.editor} aria-label={ariaLabel}>
      <CodeMirror
        value={value}
        onChange={onChange}
        height={height}
        minHeight={minHeight}
        theme={themeMode === "dark" ? githubDark : githubLight}
        extensions={extensions}
        basicSetup={{
          lineNumbers: true,
          foldGutter: false,
          highlightActiveLine: false,
          autocompletion: false,
          searchKeymap: false,
        }}
      />
    </div>
  );
}
