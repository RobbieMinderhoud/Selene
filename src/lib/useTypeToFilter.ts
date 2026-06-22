/**
 * Type-to-filter for a sidebar panel: there is no visible search box — while the
 * panel (or a row inside it) has focus, typing builds a filter string.
 * Backspace edits it, Escape clears it.
 *
 * Wire `onKeyDown` onto the panel's body wrapper (keydown bubbles up from the
 * focused row/button), render a {@link FilterIndicator} for `query`, and read
 * `query` to filter the list.
 */

import { useCallback, useState } from "react";

export interface TypeToFilter {
  /** Current filter string (empty = no filter). */
  query: string;
  /** Attach to the panel body wrapper; captures bubbled keystrokes. */
  onKeyDown: (e: React.KeyboardEvent) => void;
  /** Clear the filter. */
  clear: () => void;
}

export function useTypeToFilter(): TypeToFilter {
  const [query, setQuery] = useState("");
  const clear = useCallback(() => setQuery(""), []);

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      // Never hijack typing inside an editable field (e.g. inline rename).
      const target = e.target as HTMLElement;
      if (
        target.tagName === "INPUT" ||
        target.tagName === "TEXTAREA" ||
        target.tagName === "SELECT" ||
        target.isContentEditable
      ) {
        return;
      }
      if (e.key === "Escape") {
        if (query) {
          e.preventDefault();
          setQuery("");
        }
        return;
      }
      if (e.key === "Backspace") {
        if (query) {
          e.preventDefault();
          setQuery((q) => q.slice(0, -1));
        }
        return;
      }
      // A single printable character (no modifier combos, so shortcuts pass
      // through). Arrows/Enter/Space are left alone so rows still activate.
      if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
        e.preventDefault();
        setQuery((q) => q + e.key);
      }
    },
    [query],
  );

  return { query, onKeyDown, clear };
}
