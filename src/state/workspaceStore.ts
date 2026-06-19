/**
 * Workspace persistence: which folders are open in the sidebar, and which
 * file-backed tabs to reopen next launch.
 *
 * Like {@link useThemeStore}, this persists to `localStorage` (no backend file):
 * the manifest is tiny, non-secret, single-window, and read once at startup. We
 * persist only *paths* — never SQL content — so the file on disk stays the
 * single source of truth (its content is re-read fresh on restore).
 *
 * `openFolders` lives here; `openFiles`/`activeFile` are derived from
 * {@link useEditorStore} at write time (file-backed tabs only). Scratch tabs are
 * intentionally not persisted (only saved files reopen).
 */

import { create } from "zustand";

import { useEditorStore } from "./editorStore";

const STORAGE_KEY = "selene.workspace";

/** The persisted shape. All paths are canonical absolute paths. */
export interface WorkspaceManifest {
  openFolders: string[];
  openFiles: string[];
  activeFile: string | null;
}

interface WorkspaceState {
  /** Folder roots shown in the sidebar's Files section. */
  openFolders: string[];
  /** Add a folder root (idempotent). */
  addFolder: (path: string) => void;
  /** Remove a folder root. */
  removeFolder: (path: string) => void;
}

export const useWorkspaceStore = create<WorkspaceState>((set) => ({
  // Starts empty; App's restore effect is the single source that repopulates
  // folders (and starts watching them) from the persisted manifest.
  openFolders: [],
  addFolder: (path) =>
    set((s) =>
      s.openFolders.includes(path)
        ? s
        : { openFolders: [...s.openFolders, path] },
    ),
  removeFolder: (path) =>
    set((s) => ({ openFolders: s.openFolders.filter((p) => p !== path) })),
}));

const EMPTY: WorkspaceManifest = {
  openFolders: [],
  openFiles: [],
  activeFile: null,
};

/** Read + validate the persisted manifest. Malformed/absent => empty. */
export function readWorkspace(): WorkspaceManifest {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return EMPTY;
    const parsed = JSON.parse(raw) as Partial<WorkspaceManifest>;
    const strings = (v: unknown): string[] =>
      Array.isArray(v)
        ? v.filter((x): x is string => typeof x === "string")
        : [];
    return {
      openFolders: strings(parsed.openFolders),
      openFiles: strings(parsed.openFiles),
      activeFile:
        typeof parsed.activeFile === "string" ? parsed.activeFile : null,
    };
  } catch {
    return EMPTY;
  }
}

/** Snapshot the current manifest from the live stores (file-backed tabs only). */
export function currentManifest(): WorkspaceManifest {
  const ed = useEditorStore.getState();
  const openFiles = ed.tabs
    .map((t) => t.filePath)
    .filter((p): p is string => p !== null);
  const active = ed.tabs.find((t) => t.id === ed.activeTabId);
  return {
    openFolders: useWorkspaceStore.getState().openFolders,
    openFiles,
    activeFile: active?.filePath ?? null,
  };
}

// --- persistence wiring ----------------------------------------------------

const DEBOUNCE_MS = 250;
let lastSerialized = "";
let writeTimer: ReturnType<typeof setTimeout> | null = null;

function writeManifest(): void {
  const serialized = JSON.stringify(currentManifest());
  // Only manifest-relevant changes write — typing (which mutates `sql` but not
  // `filePath`/`activeTabId`) produces an identical snapshot and is skipped.
  if (serialized === lastSerialized) return;
  lastSerialized = serialized;
  try {
    localStorage.setItem(STORAGE_KEY, serialized);
  } catch {
    // localStorage unavailable/full — non-fatal; the session just won't restore.
  }
}

function scheduleWrite(): void {
  if (writeTimer) clearTimeout(writeTimer);
  writeTimer = setTimeout(writeManifest, DEBOUNCE_MS);
}

/**
 * Start persisting the workspace manifest on store changes (debounced). Call
 * once, after the startup restore has populated the stores, so the baseline is
 * the restored state and restore churn doesn't write. Returns a teardown fn.
 */
export function startWorkspacePersistence(): () => void {
  lastSerialized = JSON.stringify(currentManifest());
  const unsubEditor = useEditorStore.subscribe(scheduleWrite);
  const unsubWorkspace = useWorkspaceStore.subscribe(scheduleWrite);
  return () => {
    unsubEditor();
    unsubWorkspace();
    if (writeTimer) clearTimeout(writeTimer);
    writeTimer = null;
  };
}
