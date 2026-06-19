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

import { fsWatch } from "./ipc/commands";
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
import { useEditorStore } from "./state/editorStore";
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
import styles from "./App.module.css";

// The editor pane pulls in CodeMirror + the data grid (the bulk of the JS), so
// it is lazy-loaded to keep the initial shell light.
const EditorPane = lazy(() =>
  import("./components/EditorPane").then((m) => ({ default: m.EditorPane })),
);

export default function App() {
  const tabs = useEditorStore((s) => s.tabs);
  const activeTabId = useEditorStore((s) => s.activeTabId);
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

  // Restore last session, then start persistence + file-sync. Guarded so it
  // runs exactly once even under StrictMode's double-invoked effects (which
  // would otherwise install duplicate listeners).
  const booted = useRef(false);
  useEffect(() => {
    if (booted.current) return;
    booted.current = true;

    let disposeSync: (() => void) | undefined;
    let disposePersist: (() => void) | undefined;

    void (async () => {
      const manifest = readWorkspace();

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

    const isMac = /mac/i.test(navigator.platform || navigator.userAgent);
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
      }
    };
    if (!isMac) window.addEventListener("keydown", onKey);

    return () => {
      subs.forEach((p) => p.then((fn) => fn()));
      if (!isMac) window.removeEventListener("keydown", onKey);
    };
  }, []);

  return (
    <div className={styles.app}>
      <header className={styles.titlebar}>
        <div className={styles.brand}>
          <LogoMark size={18} className={styles.logo} aria-hidden />
          <span className={styles.product}>Selene</span>
          <span className={styles.tagline}>SQL editor</span>
        </div>
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
              fallback={<div className={styles.emptyWork}>Loading editor…</div>}
            >
              <EditorPane key={activeTabId} tabId={activeTabId} />
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
