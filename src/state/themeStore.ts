/** Theme selection, persisted to localStorage and applied to <html>. */

import { create } from "zustand";

import { prefersReducedMotion } from "../lib/motion";

export type ThemeMode = "dark" | "light" | "retro";

/** `document` augmented with the (still newish) View Transitions API. */
type ViewTransitionDocument = Document & {
  startViewTransition?: (callback: () => void) => unknown;
};

const STORAGE_KEY = "selene.theme";

function initialTheme(): ThemeMode {
  const stored = localStorage.getItem(STORAGE_KEY);
  if (stored === "light" || stored === "retro") return stored;
  return "dark";
}

function applyTheme(mode: ThemeMode): void {
  document.documentElement.setAttribute("data-theme", mode);
}

interface ThemeState {
  mode: ThemeMode;
  setTheme: (mode: ThemeMode) => void;
}

export const useThemeStore = create<ThemeState>((set) => {
  // Apply once at store creation so the UI never flashes the wrong theme.
  applyTheme(initialTheme());
  return {
    mode: initialTheme(),
    setTheme: (next: ThemeMode) => {
      localStorage.setItem(STORAGE_KEY, next);
      // The palette swap is the synchronous `data-theme` flip; flipping it
      // inside a view transition lets the WebView crossfade the whole UI
      // between colour schemes. Falls back to an instant swap when the API is
      // missing (older WebViews) or the user prefers reduced motion.
      const apply = () => {
        applyTheme(next);
        set({ mode: next });
      };
      const doc = document as ViewTransitionDocument;
      if (
        typeof doc.startViewTransition === "function" &&
        !prefersReducedMotion()
      ) {
        doc.startViewTransition(apply);
      } else {
        apply();
      }
    },
  };
});
