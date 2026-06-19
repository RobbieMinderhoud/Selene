/**
 * Vitest setup shared by all jsdom test files.
 *
 * - Registers `@testing-library/jest-dom` matchers (`toBeInTheDocument`, ...).
 * - Unmounts React trees and clears the document body after every test so
 *   component tests stay isolated (RTL's auto-cleanup is not assumed).
 * - Ensures bare `localStorage` references use a complete in-memory `Storage`.
 *   Some Node/Vitest launches expose an incomplete Node global instead, which
 *   lacks methods like `clear()` and breaks storage-focused unit tests.
 */

import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => Array.from(values.keys())[index] ?? null,
    removeItem: (key) => values.delete(key),
    setItem: (key, value) => values.set(key, String(value)),
  };
}

if (
  typeof globalThis.localStorage?.clear !== "function" ||
  typeof globalThis.localStorage?.setItem !== "function"
) {
  const storage = memoryStorage();
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: storage,
  });
  if (typeof window !== "undefined") {
    Object.defineProperty(window, "localStorage", {
      configurable: true,
      value: storage,
    });
  }
}

afterEach(() => {
  cleanup();
});
