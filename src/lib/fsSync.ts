/**
 * Two-way file sync: keep open file-backed tabs in step with disk.
 *
 * The backend watches workspace folders and emits a global `"fs:change"` event
 * per `.sql` path. We coalesce a burst (an editor/agent save fires several OS
 * events) into one refresh per path, re-read the file, and reconcile:
 *
 *  - disk matches our last-saved bytes  -> **noop** (this is the echo of our own
 *    save, or a no-op event). This content comparison is the self-write
 *    suppression — exact and stateless, no backend timers.
 *  - buffer clean, disk differs         -> **reload** the tab (+ a subtle toast).
 *  - buffer dirty, disk differs         -> **conflict**: queue a modal so the
 *    user chooses; we never silently clobber unsaved edits.
 *
 * A path not matching any open tab but living under a watched folder just means
 * the folder's listing changed (file added/removed) — we invalidate that dir's
 * query so the sidebar tree refreshes.
 */

import { listen } from "@tauri-apps/api/event";
import { create } from "zustand";

import { fileRead } from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import type { FsEvent } from "../ipc/types";
import { useEditorStore } from "../state/editorStore";
import { toastInfo } from "../state/toastStore";
import { basename, parentDir } from "./path";
import { qk } from "./queries";
import { queryClient } from "./queryClient";

/** The minimal tab shape the decision needs (also the unit-test surface). */
export interface ReconcileTab {
  sql: string;
  savedSql: string | null;
}

export type ReconcileDecision = "noop" | "reload" | "conflict";

/**
 * Decide what to do when `onDisk` is the file's current content and `tab` is the
 * open tab for it. Pure — no IPC, no store — so it is exhaustively unit-tested.
 */
export function reconcile(
  tab: ReconcileTab,
  onDisk: string,
): ReconcileDecision {
  if (onDisk === tab.savedSql) return "noop";
  const dirty = tab.savedSql !== null && tab.sql !== tab.savedSql;
  return dirty ? "conflict" : "reload";
}

// --- conflict queue (drives ConflictModal) ---------------------------------

export interface FileConflict {
  tabId: string;
  path: string;
  /** Latest on-disk content, applied if the user chooses "reload". */
  onDisk: string;
}

interface ConflictState {
  queue: FileConflict[];
  /** Add a conflict, or refresh the pending one for the same path (latest wins). */
  enqueue: (c: FileConflict) => void;
  /** Resolve the front conflict and advance the queue. */
  resolveFront: (choice: "reload" | "keep") => void;
}

export const useConflictStore = create<ConflictState>((set) => ({
  queue: [],
  enqueue: (c) =>
    set((s) => {
      const idx = s.queue.findIndex((q) => q.path === c.path);
      if (idx >= 0) {
        // Already prompting for this path — refresh its onDisk to the latest so
        // "reload" applies the newest content (and we don't stack modals).
        const queue = s.queue.slice();
        queue[idx] = c;
        return { queue };
      }
      return { queue: [...s.queue, c] };
    }),
  resolveFront: (choice) =>
    set((s) => {
      const [front, ...rest] = s.queue;
      // "keep": leave the buffer dirty; the user's next save wins.
      if (front && choice === "reload") {
        useEditorStore.getState().reloadTabFromDisk(front.tabId, front.onDisk);
      }
      return { queue: rest };
    }),
}));

// --- listener + per-path coalescing ----------------------------------------

const COALESCE_MS = 100;
const timers = new Map<string, ReturnType<typeof setTimeout>>();

function findTabByPath(path: string) {
  return useEditorStore.getState().tabs.find((t) => t.filePath === path);
}

function invalidateDir(path: string): void {
  void queryClient.invalidateQueries({ queryKey: qk.dir(parentDir(path)) });
}

async function refresh(path: string): Promise<void> {
  // Not an open tab → it's a tree change (file added/removed in a watched dir).
  if (!findTabByPath(path)) {
    invalidateDir(path);
    return;
  }

  let onDisk: string;
  try {
    onDisk = await fileRead(path);
  } catch (e) {
    if (asIpcError(e).kind === "not_found") {
      const gone = findTabByPath(path);
      if (gone) useEditorStore.getState().setTabFileMissing(gone.id, true);
      invalidateDir(path);
    }
    // Other read errors are transient; leave the tab as-is.
    return;
  }

  // Re-find after the await — the tab may have been closed meanwhile.
  const tab = findTabByPath(path);
  if (!tab) return;

  switch (reconcile(tab, onDisk)) {
    case "noop":
      return;
    case "reload":
      useEditorStore.getState().reloadTabFromDisk(tab.id, onDisk);
      toastInfo(`Reloaded ${basename(path)} from disk`);
      return;
    case "conflict":
      useConflictStore.getState().enqueue({ tabId: tab.id, path, onDisk });
      return;
  }
}

function scheduleRefresh(path: string): void {
  const existing = timers.get(path);
  if (existing) clearTimeout(existing);
  timers.set(
    path,
    setTimeout(() => {
      timers.delete(path);
      void refresh(path);
    }, COALESCE_MS),
  );
}

/**
 * Begin reacting to backend file-change events. Call once at app startup;
 * returns a teardown that removes the listener and clears pending timers.
 */
export async function startFileSync(): Promise<() => void> {
  const unlisten = await listen<FsEvent>("fs:change", (event) => {
    scheduleRefresh(event.payload.path);
  });
  return () => {
    unlisten();
    for (const t of timers.values()) clearTimeout(t);
    timers.clear();
  };
}
