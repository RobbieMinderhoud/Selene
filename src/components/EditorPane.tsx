/**
 * The main work area for one tab: a toolbar (session picker + Run/Cancel), the
 * SQL editor, and the results panel below, separated by a drag splitter.
 *
 * Owns the guard-modal bridge: `runQuery` calls back into local resolver state
 * to show the confirm/block modal, and resolves the user's choice.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { sessionCurrentDatabase, sessionUseDatabase } from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import type { GuardVerdict } from "../ipc/types";
import { cancelQuery, runQuery } from "../lib/runQuery";
import { useConnections, useSchemaTables } from "../lib/queries";
import { makeSchemaCompletionSource } from "../lib/sqlCompletion";
import { connectTabToConnection } from "../lib/tabSession";
import { useEditorStore } from "../state/editorStore";
import { selectSession, useSessionStore } from "../state/sessionStore";
import { useSettingsStore } from "../state/settingsStore";
import { toastError } from "../state/toastStore";
import { DatabaseSelect } from "./DatabaseSelect";
import { GuardModal } from "./GuardModal";
import { CancelIcon, RunIcon } from "./icons";
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

  // Editor/results split (percentage of height for the editor).
  const [editorPct, setEditorPct] = useState(42);
  const splitRef = useRef<HTMLDivElement>(null);

  const isRunning = status === "running";
  const canRun = !!tab?.sessionId && !isRunning;

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
      if (!tab?.sessionId) return;
      await runQuery({
        tabId,
        sessionId: tab.sessionId,
        sql: sqlText,
        readOnly: session?.readOnly ?? false,
        onBlock: (verdict) => setGuard({ kind: "block", verdict }),
        onConfirm: (verdict) =>
          new Promise<boolean>((resolve) => {
            confirmResolver.current = resolve;
            setGuard({ kind: "confirm", verdict });
          }),
      });
    },
    [tab?.sessionId, tabId, session?.readOnly],
  );

  const runWhole = useCallback(() => {
    if (tab) void handleRun(tab.sql);
  }, [tab, handleRun]);

  function resolveConfirm(ok: boolean) {
    setGuard(null);
    confirmResolver.current?.(ok);
    confirmResolver.current = null;
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
              value={session?.connectionId ?? ""}
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
