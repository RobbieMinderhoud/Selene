/**
 * Thin helpers around Tauri's `Channel<T>` for the two streaming commands.
 *
 * A `Channel` is created on the JS side, its `.onmessage` is wired to a
 * callback, and the channel instance is passed as a command argument; the Rust
 * side then `send`s events that arrive on `.onmessage`. We expose small factory
 * helpers so call sites never re-import `Channel` directly and the event types
 * are pinned.
 */

import { Channel } from "@tauri-apps/api/core";

import type { ExportEvent, ImportEvent, QueryEvent } from "./types";

/** Create a `Channel<QueryEvent>` wired to `onEvent`. */
export function createQueryChannel(
  onEvent: (event: QueryEvent) => void,
): Channel<QueryEvent> {
  const channel = new Channel<QueryEvent>();
  channel.onmessage = onEvent;
  return channel;
}

/** Create a `Channel<ExportEvent>` wired to `onEvent`. */
export function createExportChannel(
  onEvent: (event: ExportEvent) => void,
): Channel<ExportEvent> {
  const channel = new Channel<ExportEvent>();
  channel.onmessage = onEvent;
  return channel;
}

/** Create a `Channel<ImportEvent>` wired to `onEvent`. */
export function createImportChannel(
  onEvent: (event: ImportEvent) => void,
): Channel<ImportEvent> {
  const channel = new Channel<ImportEvent>();
  channel.onmessage = onEvent;
  return channel;
}

export { Channel };
