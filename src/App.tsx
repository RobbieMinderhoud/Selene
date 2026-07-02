/**
 * Selene — application shell.
 *
 * Layout: a slim title bar on top; below it a two-column body — the connection
 * sidebar (+ schema tree) on the left and the editor work area on the right.
 * The work area is a tab bar over the active tab's editor + results pane.
 *
 * On launch it restores the workspace folders open last session (files are
 * intentionally NOT reopened — each session starts with a clean editor), wires
 * the native-menu / keyboard file commands, and starts the two-way file-sync
 * listener.
 */

import {
  Suspense,
  lazy,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";

import { LogoMark } from "./components/LogoMark";

import { listen } from "@tauri-apps/api/event";

import { isMac, isWindows } from "./lib/platform";

import { fsWatch, setHealthCheck } from "./ipc/commands";
import {
  newQuery,
  openFileDialog,
  closeActiveTab,
  openFolderDialog,
  saveActiveTab,
  saveAllDirtyFileTabs,
  saveTabIfDirty,
} from "./lib/fileActions";
import { startFileSync } from "./lib/fsSync";
import { startSessionLostListener } from "./lib/sessionLost";
import { useEditorStore } from "./state/editorStore";
import { useSettingsStore } from "./state/settingsStore";
import {
  readWorkspace,
  startWorkspacePersistence,
  useWorkspaceStore,
} from "./state/workspaceStore";
import { useLayoutStore } from "./state/layoutStore";
import { ConflictModal } from "./components/ConflictModal";
import { CsvImportModal } from "./components/CsvImportModal";
import { PasswordPrompt } from "./components/PasswordPrompt";
import { Sidebar } from "./components/Sidebar";
import { TabBar } from "./components/TabBar";
import { Toasts } from "./components/Toasts";
import { SettingsModal } from "./components/SettingsModal";
import { WindowControls } from "./components/WindowControls";
import { SettingsIcon } from "./components/icons";
import styles from "./App.module.css";

// The editor pane pulls in CodeMirror + the data grid (the bulk of the JS), so
// it is lazy-loaded to keep the initial shell light.
const EditorPane = lazy(() =>
  import("./components/EditorPane").then((m) => ({ default: m.EditorPane })),
);

// The multi-target view also pulls in CodeMirror + the grid, so it is lazy too.
const MultiTargetPane = lazy(() =>
  import("./components/MultiTargetPane").then((m) => ({
    default: m.MultiTargetPane,
  })),
);

export default function App() {
  const tabs = useEditorStore((s) => s.tabs);
  const activeTabId = useEditorStore((s) => s.activeTabId);
  const activeKind = useEditorStore(
    (s) => s.tabs.find((t) => t.id === s.activeTabId)?.kind ?? "sql",
  );
  const addTab = useEditorStore((s) => s.addTab);
  const [settingsOpen, setSettingsOpen] = useState(false);

  const setSidebarWidth = useLayoutStore((s) => s.setSidebarWidth);
  const sidebarWidth = useLayoutStore((s) => s.sidebarWidth);

  const onSidebarResizerDown = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
      const startX = e.clientX;
      const startW = sidebarWidth;
      function onMove(ev: MouseEvent) {
        setSidebarWidth(startW + (ev.clientX - startX));
      }
      function onUp() {
        window.removeEventListener("mousemove", onMove);
        window.removeEventListener("mouseup", onUp);
        document.body.style.cursor = "";
      }
      document.body.style.cursor = "col-resize";
      window.addEventListener("mousemove", onMove);
      window.addEventListener("mouseup", onUp);
    },
    [sidebarWidth, setSidebarWidth],
  );

  // Save the tab being navigated away from when switching tabs.
  const prevActiveTabId = useRef<string | null>(null);
  useEffect(() => {
    const prev = prevActiveTabId.current;
    prevActiveTabId.current = activeTabId;
    if (prev && prev !== activeTabId) {
      void saveTabIfDirty(prev);
    }
  }, [activeTabId]);

  // Save all dirty file-backed tabs when the app window loses focus.
  useEffect(() => {
    const onBlur = () => saveAllDirtyFileTabs();
    window.addEventListener("blur", onBlur);
    return () => window.removeEventListener("blur", onBlur);
  }, []);

  // Suppress the webview's default right-click menu (the browser-y "Save as /
  // Print / Share" items, most glaring on Windows' WebView2). We keep it inside
  // editable surfaces — text inputs, textareas, and the CodeMirror editor
  // (`.cm-content` is contenteditable) — so native copy/paste still works there.
  // The app's own context menus (SchemaTree) preventDefault on their own and are
  // unaffected.
  useEffect(() => {
    const onContextMenu = (e: MouseEvent) => {
      const target = e.target as HTMLElement | null;
      if (
        target?.closest(
          'input, textarea, [contenteditable=""], [contenteditable="true"]',
        )
      ) {
        return;
      }
      e.preventDefault();
    };
    window.addEventListener("contextmenu", onContextMenu);
    return () => window.removeEventListener("contextmenu", onContextMenu);
  }, []);

  // Restore last session, then start persistence + file-sync. Guarded so it
  // runs exactly once even under StrictMode's double-invoked effects (which
  // would otherwise install duplicate listeners).
  const booted = useRef(false);
  useEffect(() => {
    if (booted.current) return;
    booted.current = true;

    let disposeSync: (() => void) | undefined;
    let disposePersist: (() => void) | undefined;
    let disposeSessionLost: (() => void) | undefined;

    void (async () => {
      const manifest = readWorkspace();

      // Push the saved health-check config to the backend heartbeat, and start
      // reacting to the sessions it auto-closes when a connection drops.
      const health = useSettingsStore.getState().connection;
      void setHealthCheck(health.healthCheck, health.healthCheckIntervalSecs);
      disposeSessionLost = await startSessionLostListener();

      // Restore the workspace folders + start watching them. Open *files* are
      // deliberately not reopened — the editor starts clean each session and the
      // restored folders let the user reopen any file from the tree on demand.
      for (const folder of manifest.openFolders) {
        useWorkspaceStore.getState().addFolder(folder);
        void fsWatch(folder).catch(() => undefined);
      }

      // Baseline persistence on the restored state, then react to disk changes.
      disposePersist = startWorkspacePersistence();
      disposeSync = await startFileSync();
    })();

    return () => {
      disposeSync?.();
      disposePersist?.();
      disposeSessionLost?.();
    };
  }, []);

  // File commands fire from the native menu (macOS) as `menu:*` events; on other
  // platforms (no native menu yet) the same commands bind to a window key
  // handler. macOS uses the menu accelerators alone, so there is no double-fire.
  useEffect(() => {
    const subs = [
      listen("menu:open-settings", () => setSettingsOpen(true)),
      listen("menu:new-query", () => newQuery()),
      listen("menu:open-file", () => void openFileDialog()),
      listen("menu:open-folder", () => void openFolderDialog()),
      listen("menu:save", () => void saveActiveTab()),
      listen("menu:save-as", () => void saveActiveTab(true)),
      listen("menu:close-tab", () => closeActiveTab()),
    ];

    const onKey = (e: KeyboardEvent) => {
      if (!e.ctrlKey && !e.metaKey) return;
      const k = e.key.toLowerCase();
      if (k === "s") {
        e.preventDefault();
        void saveActiveTab(e.shiftKey);
      } else if (k === "o") {
        e.preventDefault();
        void (e.shiftKey ? openFolderDialog() : openFileDialog());
      } else if (k === "n") {
        e.preventDefault();
        newQuery();
      } else if (k === "w") {
        e.preventDefault();
        closeActiveTab();
      } else if (k === ",") {
        // Windows/Linux have no native menu, so bind the macOS Cmd+, accelerator.
        e.preventDefault();
        setSettingsOpen(true);
      }
    };
    if (!isMac) window.addEventListener("keydown", onKey);

    // A desktop app must never reload its webview, so swallow the browser refresh
    // shortcuts (F5, Cmd/Ctrl+R) on every platform. F5 doubles as a configurable
    // "run query" shortcut; the editor keymap handles that when focused, and
    // blocking the default reload here doesn't interfere with it.
    const onRefreshKey = (e: KeyboardEvent) => {
      if (
        e.key === "F5" ||
        ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "r")
      ) {
        e.preventDefault();
      }
    };
    window.addEventListener("keydown", onRefreshKey);

    return () => {
      subs.forEach((p) => p.then((fn) => fn()));
      if (!isMac) window.removeEventListener("keydown", onKey);
      window.removeEventListener("keydown", onRefreshKey);
    };
  }, []);

  return (
    <div className={styles.app}>
      <header
        className={`${styles.titlebar}${isWindows ? ` ${styles.titlebarWindows}` : ""}`}
        // Windows has no native title bar (decorations: false); the whole strip
        // becomes the drag handle. macOS keeps its native title bar, so no
        // drag region there.
        data-tauri-drag-region={isWindows ? true : undefined}
      >
        <div className={styles.brand}>
          <LogoMark size={18} className={styles.logo} aria-hidden />
          <span className={styles.product}>Selene</span>
          <span className={styles.tagline}>SQL editor</span>
        </div>
        {isWindows && (
          <div className={styles.titlebarRight}>
            <button
              type="button"
              className={styles.settingsBtn}
              aria-label="Settings"
              title="Settings"
              onClick={() => setSettingsOpen(true)}
            >
              <SettingsIcon size={16} />
            </button>
            <WindowControls />
          </div>
        )}
      </header>

      <div className={styles.body}>
        <Sidebar />
        <div
          className={styles.sidebarResizer}
          onMouseDown={onSidebarResizerDown}
          role="separator"
          aria-orientation="vertical"
          aria-label="Resize sidebar"
        />
        <main className={styles.work}>
          <TabBar />
          {activeTabId ? (
            <Suspense
              fallback={<div className={styles.emptyWork}>Loading…</div>}
            >
              {activeKind === "multiTarget" ? (
                <MultiTargetPane key={activeTabId} tabId={activeTabId} />
              ) : (
                <EditorPane key={activeTabId} tabId={activeTabId} />
              )}
            </Suspense>
          ) : (
            <div className={styles.emptyWork}>
              <p>No open tabs.</p>
              <button
                type="button"
                className="primary"
                onClick={() => addTab(null)}
              >
                New query
              </button>
            </div>
          )}
        </main>
      </div>

      {/* Keep tab count referenced so empty-state logic is explicit. */}
      <span className="visually-hidden">{tabs.length} tabs open</span>

      <Toasts />
      <PasswordPrompt />
      <ConflictModal />
      <CsvImportModal />
      <SettingsModal
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
      />
    </div>
  );
}
