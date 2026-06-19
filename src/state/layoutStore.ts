/** Persistent sidebar layout: width, panel order, collapsed state, panel heights. */

import { create } from "zustand";

export type PanelId = "connections" | "files" | "schema";

interface LayoutPersisted {
  sidebarWidth: number;
  panelOrder: PanelId[];
  collapsed: { connections: boolean; files: boolean; schema: boolean };
  panelH: { connections: number; files: number; schema: number };
}

const STORAGE_KEY = "selene.layout";

const DEFAULTS: LayoutPersisted = {
  sidebarWidth: 280,
  panelOrder: ["connections", "files", "schema"],
  collapsed: { connections: false, files: false, schema: false },
  panelH: { connections: 200, files: 160, schema: 240 },
};

function load(): LayoutPersisted {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return structuredClone(DEFAULTS);
    const saved = JSON.parse(raw) as Partial<LayoutPersisted>;
    return {
      ...DEFAULTS,
      ...saved,
      collapsed: { ...DEFAULTS.collapsed, ...(saved.collapsed ?? {}) },
      panelH: { ...DEFAULTS.panelH, ...(saved.panelH ?? {}) },
    };
  } catch {
    return structuredClone(DEFAULTS);
  }
}

function persist(state: LayoutPersisted) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  } catch {
    /* quota exceeded */
  }
}

export interface LayoutState extends LayoutPersisted {
  setSidebarWidth: (w: number) => void;
  setPanelOrder: (order: PanelId[]) => void;
  toggleCollapsed: (panel: PanelId) => void;
  setPanelH: (panel: PanelId, h: number) => void;
}

export const useLayoutStore = create<LayoutState>((set, get) => {
  const initial = load();
  return {
    ...initial,
    setSidebarWidth: (w) => {
      const sidebarWidth = Math.min(600, Math.max(160, w));
      const s = get();
      persist({
        sidebarWidth,
        panelOrder: s.panelOrder,
        collapsed: s.collapsed,
        panelH: s.panelH,
      });
      set({ sidebarWidth });
    },
    setPanelOrder: (panelOrder) => {
      const s = get();
      persist({
        sidebarWidth: s.sidebarWidth,
        panelOrder,
        collapsed: s.collapsed,
        panelH: s.panelH,
      });
      set({ panelOrder });
    },
    toggleCollapsed: (panel) => {
      const s = get();
      const collapsed = { ...s.collapsed, [panel]: !s.collapsed[panel] };
      persist({
        sidebarWidth: s.sidebarWidth,
        panelOrder: s.panelOrder,
        collapsed,
        panelH: s.panelH,
      });
      set({ collapsed });
    },
    setPanelH: (panel, h) => {
      const clamped = Math.min(800, Math.max(80, h));
      const s = get();
      const panelH = { ...s.panelH, [panel]: clamped };
      persist({
        sidebarWidth: s.sidebarWidth,
        panelOrder: s.panelOrder,
        collapsed: s.collapsed,
        panelH,
      });
      set({ panelH });
    },
  };
});
