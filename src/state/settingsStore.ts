/** User preferences, persisted to localStorage. Deep-merged over DEFAULTS. */

import { create } from "zustand";

export type ReduceMotion = "system" | "on" | "off";
export type RowDensity = "compact" | "normal" | "comfortable";
export type NullDisplay = "NULL" | "(null)" | "·" | "";

export type CsvDelimiter = ";" | "," | "\t" | "|";
export type CsvQuoteChar = '"' | "'";
/** Import allows disabling quoting entirely for files that never quote fields. */
export type ImportQuoteChar = '"' | "'" | "none";
export type CsvQuoteStyle = "necessary" | "always" | "non_numeric" | "never";
export type CsvLineEnding = "lf" | "crlf";

export interface Settings {
  appearance: { reduceMotion: ReduceMotion };
  editor: {
    fontSize: number; // px
    tabSize: 2 | 4 | 8;
    wordWrap: boolean;
    lineNumbers: boolean;
    autocompletion: boolean;
    /** Schema-aware table/column completion (requires `autocompletion`). */
    schemaCompletion: boolean;
    bracketPairs: boolean; // matching + auto-close, combined
    upperCaseKeywords: boolean;
  };
  results: {
    /** Hard row cap sent to the backend. Must be one of the picker values. */
    defaultRowLimit: number;
    density: RowDensity;
    nullDisplay: NullDisplay;
  };
  /**
   * Last-used state of the editor find/replace overlay toggles. Persisted so the
   * overlay reopens the way the user left it. Not surfaced in SettingsModal — it
   * mirrors the overlay's own controls rather than being a separate preference.
   */
  search: {
    caseSensitive: boolean;
    regexp: boolean;
    wholeWord: boolean;
  };
  query: {
    confirmOnReadWrite: boolean;
    defaultConnectionReadOnly: boolean;
    /** Auto-connect an opened SQL file to the connection named in its file name. */
    autoConnectFromFile: boolean;
  };
  export: {
    delimiter: CsvDelimiter;
    quoteChar: CsvQuoteChar;
    quoteStyle: CsvQuoteStyle;
    lineEnding: CsvLineEnding;
    includeHeader: boolean;
    /** Prepend a UTF-8 BOM so Excel opens the file without re-encoding prompt. */
    bom: boolean;
  };
  /** Defaults for CSV import; the mapping menu seeds from these and lets the
   *  user override per import. */
  import: {
    delimiter: CsvDelimiter;
    quoteChar: ImportQuoteChar;
    /** Whether a CSV's first row is treated as a header (column names). */
    hasHeader: boolean;
    /** Treat an empty field as SQL NULL (vs. an empty string for text). */
    emptyAsNull: boolean;
    /** Abort the whole import on the first bad row (transactional). */
    atomic: boolean;
  };
}

const DEFAULTS: Settings = {
  appearance: { reduceMotion: "system" },
  editor: {
    fontSize: 13,
    tabSize: 4,
    wordWrap: true,
    lineNumbers: true,
    autocompletion: true,
    schemaCompletion: true,
    bracketPairs: true,
    upperCaseKeywords: true,
  },
  results: { defaultRowLimit: 50_000, density: "normal", nullDisplay: "NULL" },
  search: { caseSensitive: false, regexp: false, wholeWord: false },
  query: {
    confirmOnReadWrite: false,
    defaultConnectionReadOnly: false,
    autoConnectFromFile: true,
  },
  export: {
    delimiter: ";",
    quoteChar: '"',
    quoteStyle: "necessary",
    lineEnding: "crlf",
    includeHeader: true,
    bom: false,
  },
  import: {
    delimiter: ",",
    quoteChar: '"',
    hasHeader: true,
    emptyAsNull: true,
    atomic: true,
  },
};

const STORAGE_KEY = "selene.settings";

/** Recursively merges `partial` into a fresh deep clone of `base`. Only plain
 *  objects are recursed; arrays/primitives replace. Guards against saved blobs
 *  whose shape predates a newer field. */
function deepMerge<T>(base: T, partial: unknown): T {
  if (
    typeof base !== "object" ||
    base === null ||
    typeof partial !== "object" ||
    partial === null
  ) {
    return (partial as T) ?? base;
  }
  const result = structuredClone(base) as Record<string, unknown>;
  for (const key of Object.keys(partial as object)) {
    const bv = result[key];
    const pv = (partial as Record<string, unknown>)[key];
    result[key] =
      typeof bv === "object" && bv !== null && !Array.isArray(bv)
        ? deepMerge(bv, pv)
        : pv;
  }
  return result as T;
}

function load(): Settings {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return structuredClone(DEFAULTS);
    return deepMerge(DEFAULTS, JSON.parse(raw) as unknown);
  } catch {
    return structuredClone(DEFAULTS);
  }
}

export function applyReduceMotion(mode: ReduceMotion): void {
  const el = document.documentElement;
  if (mode === "system") {
    delete el.dataset.reduceMotion;
  } else {
    el.dataset.reduceMotion = mode === "on" ? "true" : "false";
  }
}

interface SettingsState extends Settings {
  set: <K extends keyof Settings>(
    section: K,
    patch: Partial<Settings[K]>,
  ) => void;
  resetSettings: () => void;
  /** Merge imported settings over DEFAULTS (same as initial load). */
  importSettings: (raw: unknown) => void;
}

export const useSettingsStore = create<SettingsState>((set, get) => {
  const initial = load();
  applyReduceMotion(initial.appearance.reduceMotion);
  return {
    ...initial,
    set: (section, patch) => {
      const next = { ...get()[section], ...patch } as Settings[typeof section];
      const merged = { ...get(), [section]: next };
      localStorage.setItem(
        STORAGE_KEY,
        JSON.stringify({
          appearance: merged.appearance,
          editor: merged.editor,
          results: merged.results,
          search: merged.search,
          query: merged.query,
          export: merged.export,
          import: merged.import,
        }),
      );
      if (section === "appearance") {
        applyReduceMotion((next as Settings["appearance"]).reduceMotion);
      }
      set({ [section]: next } as Partial<SettingsState>);
    },
    resetSettings: () => {
      localStorage.removeItem(STORAGE_KEY);
      applyReduceMotion(DEFAULTS.appearance.reduceMotion);
      set({ ...structuredClone(DEFAULTS) });
    },
    importSettings: (raw: unknown) => {
      const merged = deepMerge(DEFAULTS, raw);
      localStorage.setItem(
        STORAGE_KEY,
        JSON.stringify({
          appearance: merged.appearance,
          editor: merged.editor,
          results: merged.results,
          search: merged.search,
          query: merged.query,
          export: merged.export,
          import: merged.import,
        }),
      );
      applyReduceMotion(merged.appearance.reduceMotion);
      set({ ...merged });
    },
  };
});

export const DENSITY_TO_PX: Record<RowDensity, number> = {
  compact: 22,
  normal: 26,
  comfortable: 32,
};
