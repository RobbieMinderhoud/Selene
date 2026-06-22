/**
 * React to the backend's `"session:lost"` event: a live session whose
 * connection dropped and that the health-check heartbeat auto-closed.
 *
 * For each lost session we:
 *  - detach any editor tab bound to it (clearing `sessionId` but keeping
 *    `connectionId`, so the tab toolbar shows "disconnected" and offers a
 *    reconnect), and
 *  - remove the session from the session store, which makes a sidebar/browse
 *    session disappear from the schema tree and stops its keyed queries from
 *    being re-enabled.
 *
 * A single dropped connection usually loses several sessions at once (the
 * sidebar browse session plus one private session per open tab), so the
 * user-facing toast is de-duplicated per connection over a short window.
 */

import { listen } from "@tauri-apps/api/event";

import type { SessionLostEvent } from "../ipc/types";
import { useEditorStore } from "../state/editorStore";
import { useSessionStore } from "../state/sessionStore";
import { toastError } from "../state/toastStore";

/** Per-connection toast de-dupe window (ms). */
const TOAST_DEDUPE_MS = 4000;
const recentlyToasted = new Map<string, number>();

/** A wall-clock stamp; isolated so the dedupe logic stays testable. */
function now(): number {
  return Date.now();
}

function toastOncePerConnection(connectionId: string, name: string): void {
  const last = recentlyToasted.get(connectionId);
  const t = now();
  if (last !== undefined && t - last < TOAST_DEDUPE_MS) return;
  recentlyToasted.set(connectionId, t);
  toastError(
    "Connection lost",
    `${name} stopped responding and was disconnected. Reconnect from the tab toolbar.`,
  );
}

/** Handle one lost session. Exported for unit testing. */
export function handleSessionLost(event: SessionLostEvent): void {
  const { sessionId, connectionId } = event;

  // Display name (for the toast) while the session is still in the store.
  const lost = useSessionStore.getState().sessions[sessionId];
  const name = lost?.connectionName ?? connectionId;

  // Detach any tab pointing at this dead session (keep its connectionId so the
  // toolbar can offer a reconnect).
  for (const tab of useEditorStore.getState().tabs) {
    if (tab.sessionId === sessionId) {
      useEditorStore.getState().setTabSession(tab.id, null);
    }
  }

  // Drop it from the session store (removes a browse session from the tree).
  useSessionStore.getState().removeSession(sessionId);

  toastOncePerConnection(connectionId, name);
}

/**
 * Begin reacting to `session:lost` events. Call once at app startup; returns a
 * teardown that removes the listener.
 */
export async function startSessionLostListener(): Promise<() => void> {
  const unlisten = await listen<SessionLostEvent>("session:lost", (event) => {
    handleSessionLost(event.payload);
  });
  return () => {
    unlisten();
    recentlyToasted.clear();
  };
}
