/**
 * The main work area for one tab: a toolbar (session picker + Run/Cancel), the
 * SQL editor, and the results panel below, separated by a drag splitter.
 *
 * Owns the guard-modal bridge: `runQuery` calls back into local resolver state
 * to show the confirm/block modal, and resolves the user's choice.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { EditorView } from "@codemirror/view";

import { sessionCurrentDatabase, sessionUseDatabase } from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import type { GuardVerdict } from "../ipc/types";
import { cancelQuery, runQuery } from "../lib/runQuery";
import { useConnections, useSchemaTables } from "../lib/queries";
import { makeSchemaCompletionSource } from "../lib/sqlCompletion";
import { connectTabToConnection } from "../lib/tabSession";
import { getTab, useEditorStore } from "../state/editorStore";
import { selectSession, useSessionStore } from "../state/sessionStore";
import { useSettingsStore } from "../state/settingsStore";
import { toastError } from "../state/toastStore";
import { DatabaseSelect } from "./DatabaseSelect";
import { GuardModal } from "./GuardModal";
import { CancelIcon, ReconnectIcon, RunIcon } from "./icons";
import { ResultsPanel } from "./ResultsPanel";
import { SqlEditor } from "./SqlEditor";
import styles from "./EditorPane.module.css";

interface EditorPaneProps {
  tabId: string;
}

type GuardPrompt =
  | { kind: "confirm"; verdict: GuardVerdict }
  | { kind: "block"; verdict: GuardVerdict }
  | null;

export function EditorPane({ tabId }: EditorPaneProps) {
  const tab = useEditorStore((s) => s.tabs.find((t) => t.id === tabId));
  const setTabDatabase = useEditorStore((s) => s.setTabDatabase);
  const status = useEditorStore((s) => s.results[tabId]?.status ?? "idle");

  const session = useSessionStore((s) =>
    selectSession(s, tab?.sessionId ?? null),
  );

  // The toolbar picker chooses a *connection* (not a live session); each tab
  // then runs against its own private clone. List all saved connections so the
  // user can connect from a file tab without first visiting the sidebar.
  const { data: connections = [] } = useConnections();
  const connOptions = connections.map((c) => ({
    connectionId: c.id,
    label: `${c.name}${c.read_only ? " (read-only)" : ""}`,
  }));

  // Fetch the current database whenever the session changes (covers initial
  // connection and switching sessions). Also runs on mount if a session is
  // already set. `setTabDatabase` is stable (Zustand), so it is safe in deps.
  useEffect(() => {
    const sid = tab?.sessionId;
    if (!sid) {
      setTabDatabase(tabId, null);
      return;
    }
    let cancelled = false;
    void sessionCurrentDatabase(sid)
      .then((db) => {
        if (!cancelled) setTabDatabase(tabId, db || null);
      })
      .catch(() => {
        if (!cancelled) setTabDatabase(tabId, null);
      });
    return () => {
      cancelled = true;
    };
  }, [tab?.sessionId, tabId, setTabDatabase]);

  // Schema-aware autocomplete. Gated on driver capability + both settings + a
  // live session/database. Tables load eagerly (cheap, cached); the source
  // fetches a table's columns lazily on first reference. The source is memoized
  // on stable inputs so the editor doesn't reconfigure on every keystroke.
  const autocompletion = useSettingsStore((s) => s.editor.autocompletion);
  const schemaCompletion = useSettingsStore((s) => s.editor.schemaCompletion);
  const schemaGate =
    (session?.info.capabilities.schemas ?? false) &&
    autocompletion &&
    schemaCompletion &&
    !!tab?.sessionId &&
    !!tab?.currentDatabase;
  const tables = useSchemaTables(
    tab?.sessionId ?? null,
    tab?.currentDatabase ?? null,
    schemaGate,
  );
  const schemaSource = useMemo(() => {
    if (!schemaGate || !tab?.sessionId || !tab?.currentDatabase)
      return undefined;
    return makeSchemaCompletionSource({
      sessionId: tab.sessionId,
      database: tab.currentDatabase,
      tables,
    });
  }, [schemaGate, tab?.sessionId, tab?.currentDatabase, tables]);

  const [guard, setGuard] = useState<GuardPrompt>(null);
  // Holds the resolver for an in-flight confirm() so the modal can settle it.
  const confirmResolver = useRef<((ok: boolean) => void) | null>(null);
  // Mirrors `guard !== null` for synchronous reads inside handleRun (avoids a
  // stale closure without adding `guard` to its deps, which would churn the
  // editor's `onRun` and force a CodeMirror reconfigure).
  const guardOpenRef = useRef(false);
  guardOpenRef.current = guard !== null;
  // True while a run (incl. its guard confirm round-trip) is in flight.
  const runInFlight = useRef(false);
  // The live editor view, so we can return focus to it after a Run/guard action.
  // `view.focus()` restores scroll position; a raw DOM focus scroll-jumps the
  // editor on the macOS WebView (WebKit ignores `preventScroll`).
  const editorViewRef = useRef<EditorView | null>(null);

  // Editor/results split (percentage of height for the editor).
  const [editorPct, setEditorPct] = useState(42);
  const splitRef = useRef<HTMLDivElement>(null);

  const isRunning = status === "running";
  // Runnable when live, or when the tab remembers a connection it can reconnect
  // to (a dropped session auto-reconnects on Run, restoring the last database).
  const canRun = (!!tab?.sessionId || !!tab?.connectionId) && !isRunning;

  const handleUseDatabase = useCallback(
    async (db: string) => {
      if (!tab?.sessionId) return;
      try {
        await sessionUseDatabase(tab.sessionId, db);
        setTabDatabase(tabId, db);
      } catch (err) {
        toastError("Database switch failed", asIpcError(err).message);
        throw err; // re-throw so DatabaseSelect can revert the optimistic selection
      }
    },
    [tab?.sessionId, tabId, setTabDatabase],
  );

  const handleRun = useCallback(
    async (sqlText: string) => {
      if (!sqlText.trim()) return;
      // Re-entrancy guard. Cmd+Enter bypasses the disabled Run button, so without
      // this a second press while the guard modal is open would re-fire the run:
      // start a parallel guard check, overwrite confirmResolver, and re-open the
      // modal (the "Cmd+Enter on the guard sometimes doesn't work" symptom).
      if (runInFlight.current || guardOpenRef.current) return;
      runInFlight.current = true;
      try {
        let sessionId = tab?.sessionId ?? null;
        // The session was auto-closed (dropped link) but the tab remembers its
        // connection: reconnect — which restores the last database — then run.
        if (!sessionId && tab?.connectionId) {
          await connectTabToConnection(tabId, tab.connectionId);
          sessionId = getTab(tabId)?.sessionId ?? null;
        }
        if (!sessionId) return;

        // Read read-only from the (possibly just-reconnected) live session.
        const live = useSessionStore.getState().sessions[sessionId];
        await runQuery({
          tabId,
          sessionId,
          sql: sqlText,
          readOnly: live?.readOnly ?? session?.readOnly ?? false,
          onBlock: (verdict) => setGuard({ kind: "block", verdict }),
          onConfirm: (verdict) =>
            new Promise<boolean>((resolve) => {
              confirmResolver.current = resolve;
              setGuard({ kind: "confirm", verdict });
            }),
        });
      } finally {
        runInFlight.current = false;
      }
    },
    [tab?.sessionId, tab?.connectionId, tabId, session?.readOnly],
  );

  const runWhole = useCallback(() => {
    if (tab) void handleRun(tab.sql);
    // Keep focus in the editor after a button-click run; otherwise the button
    // holds focus and the next click into the (unfocused) editor scroll-jumps
    // on the macOS WebView. A pending guard modal re-focuses its card afterward.
    editorViewRef.current?.focus();
  }, [tab, handleRun]);

  function resolveConfirm(ok: boolean) {
    setGuard(null);
    confirmResolver.current?.(ok);
    confirmResolver.current = null;
    // Return focus to the editor (scroll-safe) instead of leaving it on the
    // dismissed modal / document body.
    editorViewRef.current?.focus();
  }

  function onSplitterDown(e: React.MouseEvent) {
    e.preventDefault();
    const container = splitRef.current;
    if (!container) return;
    const rect = container.getBoundingClientRect();
    function onMove(ev: MouseEvent) {
      const pct = ((ev.clientY - rect.top) / rect.height) * 100;
      setEditorPct(Math.min(80, Math.max(15, pct)));
    }
    function onUp() {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      document.body.style.cursor = "";
    }
    document.body.style.cursor = "row-resize";
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }

  if (!tab) return null;

  return (
    <div className={styles.pane}>
      <div ref={splitRef} className={styles.split}>
        {/* Editor island: its own toolbar (session + Run) sits atop CodeMirror.
            The percentage height lives on the island so the splitter resizes
            toolbar+editor as one unit. */}
        <section
          className={styles.editorIsland}
          style={{ height: `${editorPct}%` }}
        >
          <div className={styles.toolbar}>
            <label className="visually-hidden" htmlFor={`session-${tabId}`}>
              Session
            </label>
            <select
              id={`session-${tabId}`}
              className={styles.sessionSelect}
              value={tab.connectionId ?? session?.connectionId ?? ""}
              onChange={(e) =>
                void connectTabToConnection(tabId, e.target.value || null)
              }
            >
              <option value="">No connection</option>
              {connOptions.map((c) => (
                <option key={c.connectionId} value={c.connectionId}>
                  {c.label}
                </option>
              ))}
            </select>

            {/* Connection status. Green when the session is live; amber with a
                Reconnect action when the link dropped (the tab keeps its
                connectionId after an auto-close). */}
            {tab.connectionId &&
              (tab.sessionId ? (
                <span
                  className={styles.connStatus}
                  data-state="connected"
                  title="Connected"
                >
                  <span className={styles.statusDot} aria-hidden />
                  Connected
                </span>
              ) : (
                <span
                  className={styles.connStatus}
                  data-state="disconnected"
                  title="Connection lost"
                >
                  <span className={styles.statusDot} aria-hidden />
                  Disconnected
                  <button
                    type="button"
                    className={styles.reconnectBtn}
                    onClick={() =>
                      void connectTabToConnection(tabId, tab.connectionId)
                    }
                    title="Reconnect"
                  >
                    <ReconnectIcon />
                    Reconnect
                  </button>
                </span>
              ))}

            {session && tab.sessionId && (
              <DatabaseSelect
                sessionId={tab.sessionId}
                currentDatabase={tab.currentDatabase}
                onSelect={handleUseDatabase}
              />
            )}

            <div className={styles.runGroup}>
              <button
                type="button"
                className="primary"
                onClick={runWhole}
                disabled={!canRun}
                title="Run (Cmd/Ctrl+Enter)"
              >
                <RunIcon />
                Run
              </button>
              <button
                type="button"
                className="danger"
                onClick={() => cancelQuery(tabId)}
                disabled={!isRunning}
                title="Cancel running query"
              >
                <CancelIcon />
                Cancel
              </button>
            </div>
            {session && (
              <span className={styles.caps} title="Driver capabilities">
                {session.info.driver.toUpperCase()}
              </span>
            )}
          </div>
          <div className={styles.editorWrap}>
            <SqlEditor
              tabId={tabId}
              onRun={handleRun}
              schemaSource={schemaSource}
              viewRef={editorViewRef}
            />
          </div>
        </section>
        <div
          className={styles.splitter}
          role="separator"
          aria-orientation="horizontal"
          onMouseDown={onSplitterDown}
        />
        <section
          className={styles.resultsIsland}
          style={{ height: `${100 - editorPct}%` }}
        >
          <ResultsPanel tabId={tabId} />
        </section>
      </div>

      <GuardModal
        state={guard}
        onConfirm={() => resolveConfirm(true)}
        onCancel={() => resolveConfirm(false)}
      />
    </div>
  );
}
