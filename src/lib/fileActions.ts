/**
 * The single action layer for file commands — called by BOTH the native menu
 * (via `menu:*` events) and the cross-platform keyboard handler, so save/open
 * behaviour lives in exactly one place.
 *
 * Save is explicit (Cmd/Ctrl+S): Selene writes the file only on save, never on
 * every keystroke — deliberate, since an external agent may also be editing the
 * same file (autosave + agent = a conflict generator). The file→tab direction is
 * automatic regardless (see `fsSync.ts`).
 *
 * Auto-save (focus change): `saveTabIfDirty` / `saveAllDirtyFileTabs` are the
 * silent variants used when the window loses focus or the active tab changes.
 * They skip scratch tabs (no path yet), already-clean tabs, and tabs that are
 * waiting for the user to resolve a disk conflict — in those cases the
 * ConflictModal must win.
 */

import {
  canonicalizePath,
  fileRead,
  fileWrite,
  fsUnwatch,
  fsWatch,
} from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import { useEditorStore } from "../state/editorStore";
import { useSessionStore } from "../state/sessionStore";
import { toastError, toastSuccess } from "../state/toastStore";
import { useWorkspaceStore } from "../state/workspaceStore";
import {
  autoConnectTabFromFile,
  bindTabConnection,
  closeTabAndSession,
} from "./tabSession";
import { basename, parentDir } from "./path";
import { useConflictStore } from "./fsSync";

const SQL_FILTER = [{ name: "SQL", extensions: ["sql"] }];

function activeTab() {
  const ed = useEditorStore.getState();
  return ed.tabs.find((t) => t.id === ed.activeTabId) ?? null;
}

/**
 * Connection the active tab uses, so a new/opened tab can inherit it by cloning
 * its own private session (sessions are never shared between tabs).
 */
function activeConnectionId(): string | null {
  const sid = activeTab()?.sessionId ?? null;
  if (!sid) return null;
  return useSessionStore.getState().sessions[sid]?.connectionId ?? null;
}

function defaultSaveName(filePath: string | null, title: string): string {
  if (filePath) return filePath;
  const base = title.trim() || "query";
  return base.toLowerCase().endsWith(".sql") ? base : `${base}.sql`;
}

/** New empty scratch tab, inheriting the active tab's connection if any. */
export function newQuery(): void {
  const connectionId = activeConnectionId();
  const id = useEditorStore.getState().addTab(null);
  if (connectionId) void bindTabConnection(id, connectionId);
}

/**
 * Save a specific tab. Writes to its existing path, or (scratch tab / Save As)
 * prompts for one. After a Save As we start watching the new file's folder so
 * external edits sync back.
 *
 * Pass `silent = true` to suppress the success toast (used by auto-save paths).
 */
export async function saveTab(
  tabId: string,
  forceDialog = false,
  silent = false,
): Promise<void> {
  const ed = useEditorStore.getState();
  const tab = ed.tabs.find((t) => t.id === tabId);
  if (!tab) return;
  const content = tab.sql; // snapshot; later keystrokes remain unsaved (dirty)

  try {
    let target = tab.filePath;
    if (forceDialog || !target) {
      const { save } = await import("@tauri-apps/plugin-dialog");
      const picked = await save({
        title: "Save query",
        defaultPath: defaultSaveName(tab.filePath, tab.title),
        filters: SQL_FILTER,
      });
      if (!picked) return; // cancelled
      await fileWrite(picked, content);
      target = await canonicalizePath(picked);
      void fsWatch(parentDir(target)).catch(() => undefined);
    } else {
      await fileWrite(target, content);
    }
    ed.markTabSaved(tab.id, target, content);
    if (!silent) toastSuccess(`Saved ${basename(target)}`);
  } catch (e) {
    toastError("Could not save file", asIpcError(e).message);
  }
}

/** Save the active tab (the Cmd/Ctrl+S target). */
export function saveActiveTab(forceDialog = false): Promise<void> {
  const id = useEditorStore.getState().activeTabId;
  return id ? saveTab(id, forceDialog) : Promise.resolve();
}

/**
 * Auto-save a single tab silently if it has unsaved changes.
 *
 * Skips:
 *  - scratch tabs (no `filePath` yet — needs an explicit Save As)
 *  - clean tabs (nothing to write)
 *  - tabs waiting for conflict resolution (the ConflictModal owns that decision)
 */
export async function saveTabIfDirty(tabId: string): Promise<void> {
  const { tabs } = useEditorStore.getState();
  const tab = tabs.find((t) => t.id === tabId);
  if (!tab?.filePath) return;
  if (tab.sql === tab.savedSql) return;
  if (useConflictStore.getState().queue.some((c) => c.tabId === tabId)) return;
  await saveTab(tabId, false, true);
}

/**
 * Auto-save every dirty file-backed tab silently.
 * Called on window blur so in-flight edits are not lost when switching apps.
 */
export function saveAllDirtyFileTabs(): void {
  const { tabs } = useEditorStore.getState();
  for (const tab of tabs) {
    void saveTabIfDirty(tab.id);
  }
}

/**
 * Open a `.sql` file already identified by a canonical `path` (from the file
 * tree, or a dialog after canonicalization). Idempotent via `openFileTab`.
 *
 * If the new tab has no connection yet, we try to auto-connect it to the
 * connection whose name appears in the file name (see `autoConnectTabFromFile`).
 */
export async function openFileFromPath(path: string): Promise<void> {
  try {
    const content = await fileRead(path);
    const id = useEditorStore.getState().openFileTab(path, content);
    void fsWatch(parentDir(path)).catch(() => undefined);
    void autoConnectTabFromFile(id);
  } catch (e) {
    toastError("Could not open file", asIpcError(e).message);
  }
}

/** Prompt for a `.sql` file and open it. */
export async function openFileDialog(): Promise<void> {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      title: "Open SQL file",
      multiple: false,
      filters: SQL_FILTER,
    });
    if (typeof picked !== "string") return; // cancelled
    const path = await canonicalizePath(picked);
    await openFileFromPath(path);
  } catch (e) {
    toastError("Could not open file", asIpcError(e).message);
  }
}

/** Prompt for a folder, add it to the workspace, and watch it for changes. */
export async function openFolderDialog(): Promise<void> {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({ title: "Open folder", directory: true });
    if (typeof picked !== "string") return; // cancelled
    const path = await canonicalizePath(picked);
    useWorkspaceStore.getState().addFolder(path);
    void fsWatch(path).catch(() => undefined);
  } catch (e) {
    toastError("Could not open folder", asIpcError(e).message);
  }
}

/**
 * Request to close a tab. If it has unsaved file changes, sets `pendingCloseTabId`
 * so TabBar can show the discard-or-save confirmation modal; otherwise closes immediately.
 */
export function requestTabClose(id: string): void {
  const { tabs, setPendingCloseTabId } = useEditorStore.getState();
  const tab = tabs.find((t) => t.id === id);
  const dirty = !!tab && tab.filePath !== null && tab.sql !== tab.savedSql;
  if (dirty) {
    setPendingCloseTabId(id);
  } else {
    closeTabAndSession(id);
  }
}

/** Close the currently active tab (Cmd/Ctrl+W target). */
export function closeActiveTab(): void {
  const id = useEditorStore.getState().activeTabId;
  if (id) requestTabClose(id);
}

/** Stop showing a workspace folder and stop watching it. */
export function closeFolder(path: string): void {
  useWorkspaceStore.getState().removeFolder(path);
  void fsUnwatch(path).catch(() => undefined);
}
