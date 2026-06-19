/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [react()],

  // Vitest: component/integration tests run in a jsdom DOM with React Testing
  // Library; the setup file registers jest-dom matchers and resets the DOM and
  // module mocks between tests so each case is deterministic and isolated.
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/test/setup.ts"],
    css: false,
    // Clear mock call history and restore spies before each test so
    // module-level `vi.fn()` mocks don't accumulate calls across cases.
    clearMocks: true,
    restoreMocks: true,
  },

  build: {
    // CodeMirror and the data grid are large; split them into their own chunks
    // so the initial shell stays small and vendor code is cached across builds.
    chunkSizeWarningLimit: 800,
    rollupOptions: {
      output: {
        manualChunks: {
          codemirror: [
            "@uiw/react-codemirror",
            "@uiw/codemirror-theme-github",
            "@codemirror/lang-sql",
            "@codemirror/view",
            "@codemirror/state",
          ],
          datagrid: ["@tanstack/react-table", "@tanstack/react-virtual"],
          query: ["@tanstack/react-query"],
        },
      },
    },
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
