/**
 * Thin, safe wrapper over `@tauri-apps/plugin-log`.
 *
 * SECURITY: we only ever log already-sanitized text — error messages from
 * `IpcError` (the backend guarantees these are secret-free) and UI lifecycle
 * notes. We NEVER log SQL text, row/cell data, passwords, or connection
 * strings. The plugin's writes go to the same sinks the Rust side configured.
 *
 * Logging failures are swallowed: diagnostics must never break the UI.
 */

import { error as logError } from "@tauri-apps/plugin-log";

/** Record a sanitized error (e.g. an `IpcError.message`) to the app log. */
export function logErr(context: string, message: string): void {
  void logError(`${context}: ${message}`).catch(() => undefined);
}
