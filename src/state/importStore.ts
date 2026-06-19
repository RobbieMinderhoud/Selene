/**
 * Pending CSV-import request, set from the schema-tree context menu and consumed
 * by the single top-level `CsvImportModal`. Decouples the deep tree trigger from
 * the modal (mirrors how ConflictModal / PasswordPrompt read their own stores).
 *
 * Holds only non-secret tree coordinates (session id, database/schema/table).
 */

import { create } from "zustand";

export type ImportMode = "existing" | "new";

export interface ImportRequest {
  sessionId: string;
  /** Database context (always known from the tree node that was clicked). */
  database: string;
  schema: string;
  mode: ImportMode;
  /** Target table — present for "existing"; for "new" the user names it. */
  table?: string;
}

interface ImportState {
  request: ImportRequest | null;
  requestImport: (req: ImportRequest) => void;
  closeImport: () => void;
}

export const useImportStore = create<ImportState>((set) => ({
  request: null,
  requestImport: (req) => set({ request: req }),
  closeImport: () => set({ request: null }),
}));
