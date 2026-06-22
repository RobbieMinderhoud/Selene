/**
 * Editor tabs + per-tab streaming result state.
 *
 * ## Render-isolation design (STEP 5 requirement)
 * Streamed `rows` events can arrive hundreds of times for one query. Appending a
 * batch must NOT re-render the CodeMirror editor (which only cares about `sql`).
 * We achieve that with two mechanisms:
 *
 *  1. **Separate slices.** Tab text/metadata (`tabs`) and result buffers
 *     (`results`) are distinct maps. Components select narrowly:
 *     the editor selects `tabs[id].sql`; the grid selects from `results[id]`.
 *
 *  2. **Mutable buffers + a version counter.** Each result set's `rows` array is
 *     mutated in place on append (cheap, no per-batch array copy of all rows),
 *     and only a small `rev` integer on the ResultState is bumped to signal "new
 *     data". The grid subscribes to `rev` (and `status`), so an append triggers a
 *     re-render of the grid alone — never the editor. Columns/status are replaced
 *     immutably so their selectors fire only on real change.
 *
 * This keeps a million-row stream smooth while the editor stays still.
 */

import { create } from "zustand";

import type { Column, ExecOutcome } from "../ipc/types";
import type { CellValue } from "../ipc/types";
import { basename } from "../lib/path";

export type RunStatus = "idle" | "running" | "done" | "cancelled" | "failed";

/** One result set within a (possibly multi-set) query result. */
export interface ResultSet {
  setIndex: number;
  columns: Column[];
  /** Mutated in place on append; the grid reads it via `rev`. */
  rows: CellValue[][];
  /** Rows affected for DML sets (from `setEnd`); `null` for SELECTs. */
  affected: number | null;
}

/** Streaming result state for a single tab. */
export interface ResultState {
  status: RunStatus;
  /**
   * Stable id for this run, assigned at reset/start and never cleared. Used to
   * key the grid so a *new* query resets the virtualizer while appends within
   * one run reuse it. (Distinct from `queryId`, which is nulled on finish.)
   */
  runId: number;
  queryId: string | null;
  resultSets: ResultSet[];
  /** Active result-set sub-tab index. */
  activeSet: number;
  /** Total rows across all sets, for the status bar. */
  rowCount: number;
  elapsedMs: number | null;
  truncated: boolean;
  /**
   * True when the batch was a rollback-wrapped dry-run
   * (`BEGIN TRAN; <DML …>; ROLLBACK`): the affected counts are what *would*
   * have changed and nothing was committed. Drives the "rolled back" label.
   */
  rolledBack: boolean;
  /** Error message when `status === "failed"`. */
  error: string | null;
  /**
   * Monotonic revision bumped on every mutation (row append, set add, status
   * change). The grid subscribes to this to know when to repaint.
   */
  rev: number;
}

export interface EditorTab {
  id: string;
  title: string;
  sql: string;
  /** Session this tab runs against; `null` until a connection is chosen. */
  sessionId: string | null;
  /**
   * The connection this tab is bound to, independent of the live session. Unlike
   * `sessionId` it survives a dropped/auto-closed session, so the toolbar can
   * show "disconnected from X" and offer a reconnect. Cleared only when the user
   * explicitly detaches the tab ("No connection").
   */
  connectionId: string | null;
  /** Current database for this tab's session; updated after queries and USE statements. */
  currentDatabase: string | null;
  /**
   * The last database this tab was actually in (mirrors `currentDatabase` but is
   * NOT cleared when the session drops). On a reconnect to the same connection
   * it is restored with a `USE`, so running a query after a dropped link lands
   * back in the same database instead of the connection's default. Reset when
   * the tab is bound to a different connection.
   */
  lastDatabase: string | null;
  /** Canonical absolute path once saved/opened; `null` for an unsaved scratch tab. */
  filePath: string | null;
  /**
   * The exact on-disk bytes as of the last read/write. Dirty state is derived
   * from `sql !== savedSql` (see {@link selectDirty}); this is also what the
   * file-sync reconciler compares against to suppress echoes of our own writes.
   * `null` for a scratch tab.
   */
  savedSql: string | null;
  /** Set when the backing file was deleted/renamed on disk while open. */
  fileMissing: boolean;
}

let runSeq = 0;
function nextRunId(): number {
  runSeq += 1;
  return runSeq;
}

function emptyResult(): ResultState {
  return {
    status: "idle",
    runId: nextRunId(),
    queryId: null,
    resultSets: [],
    activeSet: 0,
    rowCount: 0,
    elapsedMs: null,
    truncated: false,
    rolledBack: false,
    error: null,
    rev: 0,
  };
}

let tabSeq = 0;
function nextTabId(): string {
  tabSeq += 1;
  return `tab-${tabSeq}`;
}

interface EditorState {
  tabs: EditorTab[];
  activeTabId: string | null;
  results: Record<string, ResultState>;
  /** Tab awaiting close confirmation (dirty unsaved file). Set by requestTabClose; cleared by TabBar modal. */
  pendingCloseTabId: string | null;

  // --- tab lifecycle ---
  addTab: (sessionId?: string | null, sql?: string) => string;
  closeTab: (id: string) => void;
  setPendingCloseTabId: (id: string | null) => void;
  setActiveTab: (id: string) => void;
  setSql: (id: string, sql: string) => void;
  setTabSession: (id: string, sessionId: string | null) => void;
  /** Bind/clear the tab's intended connection (kept across session drops). */
  setTabConnection: (id: string, connectionId: string | null) => void;
  setTabDatabase: (id: string, db: string | null) => void;
  renameTab: (id: string, title: string) => void;
  /** Insert text at (replacing) — used by the schema tree's double-click. */
  appendSql: (id: string, text: string) => void;

  // --- file-backed tabs ---
  /**
   * Open `filePath` (already read into `content`) as a tab. Idempotent on the
   * canonical path: if a tab for it is already open, just focus it (the buffer
   * is left untouched so unsaved edits survive a re-open) and return its id.
   * `sessionId` lets a file opened during a live session inherit that session.
   */
  openFileTab: (
    filePath: string,
    content: string,
    sessionId?: string | null,
  ) => string;
  /** Bind a tab to `filePath` after a successful Save / Save As. */
  markTabSaved: (id: string, filePath: string, content: string) => void;
  /** Replace the buffer with fresh disk content (external change accepted). */
  reloadTabFromDisk: (id: string, content: string) => void;
  /** Flag/unflag that the backing file is gone on disk. */
  setTabFileMissing: (id: string, missing: boolean) => void;

  // --- result-set sub-tab ---
  setActiveSet: (id: string, setIndex: number) => void;

  // --- streaming result mutations (driven by the query channel) ---
  resetResult: (id: string) => void;
  resultStarted: (id: string, queryId: string) => void;
  resultMeta: (id: string, setIndex: number, columns: Column[]) => void;
  resultAppendRows: (id: string, setIndex: number, rows: CellValue[][]) => void;
  resultSetEnd: (id: string, setIndex: number, affected: number | null) => void;
  resultFinished: (id: string, outcome: ExecOutcome, elapsedMs: number) => void;
  resultCancelled: (id: string) => void;
  resultFailed: (id: string, message: string) => void;
}

/** Replace one tab's ResultState immutably while bumping `rev`. */
function patchResult(
  state: EditorState,
  id: string,
  patch: (prev: ResultState) => ResultState,
): Partial<EditorState> {
  const prev = state.results[id] ?? emptyResult();
  const next = patch(prev);
  return { results: { ...state.results, [id]: next } };
}

export const useEditorStore = create<EditorState>((set, get) => ({
  tabs: [],
  activeTabId: null,
  results: {},
  pendingCloseTabId: null,

  addTab: (sessionId = null, sql = "") => {
    const id = nextTabId();
    set((state) => {
      const n = state.tabs.length + 1;
      const tab: EditorTab = {
        id,
        title: `Query ${n}`,
        sql,
        sessionId,
        connectionId: null,
        currentDatabase: null,
        lastDatabase: null,
        filePath: null,
        savedSql: null,
        fileMissing: false,
      };
      return {
        tabs: [...state.tabs, tab],
        activeTabId: id,
        results: { ...state.results, [id]: emptyResult() },
      };
    });
    return id;
  },

  openFileTab: (filePath, content, sessionId = null) => {
    // Already open? Focus it without touching the buffer (don't clobber unsaved
    // edits, and stay idempotent under StrictMode's double-invoked effects).
    const existing = get().tabs.find((t) => t.filePath === filePath);
    if (existing) {
      set({ activeTabId: existing.id });
      return existing.id;
    }
    const id = nextTabId();
    const tab: EditorTab = {
      id,
      title: basename(filePath),
      sql: content,
      sessionId,
      connectionId: null,
      currentDatabase: null,
      lastDatabase: null,
      filePath,
      savedSql: content,
      fileMissing: false,
    };
    set((state) => ({
      tabs: [...state.tabs, tab],
      activeTabId: id,
      results: { ...state.results, [id]: emptyResult() },
    }));
    return id;
  },

  markTabSaved: (id, filePath, content) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id
          ? {
              ...t,
              filePath,
              savedSql: content,
              title: basename(filePath),
              fileMissing: false,
            }
          : t,
      ),
    })),

  reloadTabFromDisk: (id, content) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id
          ? { ...t, sql: content, savedSql: content, fileMissing: false }
          : t,
      ),
    })),

  setTabFileMissing: (id, missing) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id ? { ...t, fileMissing: missing } : t,
      ),
    })),

  closeTab: (id) =>
    set((state) => {
      const idx = state.tabs.findIndex((t) => t.id === id);
      if (idx === -1) return state;
      const tabs = state.tabs.filter((t) => t.id !== id);
      const results = { ...state.results };
      delete results[id];
      let activeTabId = state.activeTabId;
      if (activeTabId === id) {
        const fallback = tabs[idx] ?? tabs[idx - 1] ?? tabs[0] ?? null;
        activeTabId = fallback ? fallback.id : null;
      }
      return { tabs, results, activeTabId };
    }),

  setActiveTab: (id) => set({ activeTabId: id }),

  setPendingCloseTabId: (id) => set({ pendingCloseTabId: id }),

  setSql: (id, sql) =>
    set((state) => ({
      tabs: state.tabs.map((t) => (t.id === id ? { ...t, sql } : t)),
    })),

  setTabSession: (id, sessionId) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id ? { ...t, sessionId, currentDatabase: null } : t,
      ),
    })),

  setTabConnection: (id, connectionId) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id
          ? {
              ...t,
              connectionId,
              // Binding to a *different* connection invalidates the remembered
              // database (it belonged to the old server). Reconnecting to the
              // same connection keeps it so it can be restored.
              ...(connectionId !== t.connectionId
                ? { lastDatabase: null }
                : {}),
            }
          : t,
      ),
    })),

  setTabDatabase: (id, db) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id
          ? {
              ...t,
              currentDatabase: db,
              // Remember the last real database so a reconnect can restore it;
              // a null (session cleared) must not erase that memory.
              ...(db ? { lastDatabase: db } : {}),
            }
          : t,
      ),
    })),

  renameTab: (id, title) =>
    set((state) => ({
      tabs: state.tabs.map((t) => (t.id === id ? { ...t, title } : t)),
    })),

  appendSql: (id, text) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id
          ? {
              ...t,
              sql: t.sql.trim().length ? `${t.sql.trimEnd()}\n${text}` : text,
            }
          : t,
      ),
    })),

  setActiveSet: (id, setIndex) =>
    set((state) =>
      patchResult(state, id, (prev) => ({
        ...prev,
        activeSet: setIndex,
        rev: prev.rev + 1,
      })),
    ),

  resetResult: (id) =>
    set((state) => ({ results: { ...state.results, [id]: emptyResult() } })),

  resultStarted: (id, queryId) =>
    // Non-destructive: `resetResult` already cleared the slot before the channel
    // opened, so we only transition to running + record the queryId. We must NOT
    // re-clear here — `meta`/`rows` events can race ahead of this call, and
    // wiping would drop already-buffered rows.
    //
    // Guard against overwriting a terminal state: for very fast queries (e.g.
    // USE) the `finished` event can arrive before `queryRun` resolves and calls
    // `resultStarted` a second time. Blindly setting "running" would overwrite
    // "done" and leave the results panel stuck.
    set((state) =>
      patchResult(state, id, (prev) => ({
        ...prev,
        status: prev.status === "idle" ? "running" : prev.status,
        queryId: prev.queryId ?? queryId,
        rev: prev.rev + 1,
      })),
    ),

  resultMeta: (id, setIndex, columns) =>
    set((state) =>
      patchResult(state, id, (prev) => {
        const resultSets = prev.resultSets.slice();
        const existing = resultSets.findIndex((r) => r.setIndex === setIndex);
        const rs: ResultSet = { setIndex, columns, rows: [], affected: null };
        if (existing === -1) resultSets.push(rs);
        else resultSets[existing] = rs;
        return { ...prev, resultSets, rev: prev.rev + 1 };
      }),
    ),

  resultAppendRows: (id, setIndex, rows) =>
    set((state) =>
      patchResult(state, id, (prev) => {
        const resultSets = prev.resultSets.slice();
        let target = resultSets.find((r) => r.setIndex === setIndex);
        if (!target) {
          // Rows before meta (shouldn't happen, but stay robust).
          target = { setIndex, columns: [], rows: [], affected: null };
          resultSets.push(target);
        }
        // Mutate the buffer in place (cheap); signal via `rev`.
        for (const row of rows) target.rows.push(row);
        return {
          ...prev,
          resultSets,
          rowCount: prev.rowCount + rows.length,
          rev: prev.rev + 1,
        };
      }),
    ),

  resultSetEnd: (id, setIndex, affected) =>
    set((state) =>
      patchResult(state, id, (prev) => {
        const resultSets = prev.resultSets.map((r) =>
          r.setIndex === setIndex ? { ...r, affected } : r,
        );
        return { ...prev, resultSets, rev: prev.rev + 1 };
      }),
    ),

  resultFinished: (id, outcome, elapsedMs) =>
    set((state) =>
      patchResult(state, id, (prev) => ({
        ...prev,
        status: "done",
        queryId: null,
        elapsedMs,
        truncated: outcome.truncated,
        rolledBack: outcome.rolled_back,
        // Prefer the authoritative server count if present.
        rowCount: outcome.total_rows ?? prev.rowCount,
        rev: prev.rev + 1,
      })),
    ),

  resultCancelled: (id) =>
    set((state) =>
      patchResult(state, id, (prev) => ({
        ...prev,
        status: "cancelled",
        queryId: null,
        rev: prev.rev + 1,
      })),
    ),

  resultFailed: (id, message) =>
    set((state) =>
      patchResult(state, id, (prev) => ({
        ...prev,
        status: "failed",
        queryId: null,
        error: message,
        rev: prev.rev + 1,
      })),
    ),
}));

// --- Narrow selectors (keep components from over-subscribing) -------------

export const selectActiveTab = (s: EditorState): EditorTab | undefined =>
  s.tabs.find((t) => t.id === s.activeTabId);

/** Get a tab's SQL (editor subscribes to this only). */
export function selectSql(s: EditorState, id: string | null): string {
  if (!id) return "";
  return s.tabs.find((t) => t.id === id)?.sql ?? "";
}

/**
 * Whether a tab has unsaved edits. Derived (never stored) so there is no third
 * source of truth to drift on every keystroke. A scratch tab (no `filePath`) is
 * never "dirty" — only file-backed tabs are persisted/saved.
 */
export function selectDirty(s: EditorState, id: string | null): boolean {
  if (!id) return false;
  const t = s.tabs.find((tab) => tab.id === id);
  return !!t && t.filePath !== null && t.sql !== t.savedSql;
}

/** Stable getter (used outside React, e.g. in the run flow). */
export function getTab(id: string): EditorTab | undefined {
  return useEditorStore.getState().tabs.find((t) => t.id === id);
}
