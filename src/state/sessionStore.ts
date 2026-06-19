/**
 * Live sessions, keyed by `sessionId`. A session is created on
 * `session_connect` and removed on `session_disconnect`. We keep the originating
 * connection name + the spec's `read_only` flag alongside the `SessionInfo` so
 * the schema tree and run-flow have what they need without another round-trip.
 *
 * Sessions come in two flavours (see `kind`):
 *  - **browse**: opened from the sidebar; one per connected connection. These
 *    back the schema tree and the toolbar's connection list, and are the only
 *    sessions listed in `order`.
 *  - **tab**: a private execution session cloned for a single editor tab so its
 *    database context (a `USE [db]`) can never leak into another tab sharing the
 *    same connection. Kept out of `order` so they stay hidden from those lists.
 *
 * Note: this holds only non-secret data (ids, capabilities, read-only flag).
 * Passwords never enter any store.
 */

import { create } from "zustand";

import type { SessionInfo } from "../ipc/types";

export interface LiveSession {
  info: SessionInfo;
  /** Connection id this session was opened from. */
  connectionId: string;
  /** Display name (from the connection spec) for the tree header. */
  connectionName: string;
  /** Read-only flag from the spec, used to drive the guard's `readOnly`. */
  readOnly: boolean;
  /**
   * Whether this is a shared sidebar/browse session or a tab-private execution
   * session. Defaults to "browse" when omitted (back-compat for callers/tests
   * that predate per-tab sessions).
   */
  kind?: "browse" | "tab";
}

interface SessionState {
  sessions: Record<string, LiveSession>;
  /** Ordered session ids (most-recent last) for stable rendering. */
  order: string[];
  addSession: (session: LiveSession) => void;
  removeSession: (sessionId: string) => void;
}

export const useSessionStore = create<SessionState>((set) => ({
  sessions: {},
  order: [],
  addSession: (session) =>
    set((state) => {
      const id = session.info.sessionId;
      // Only browse sessions are listed (sidebar tree + connection dropdown);
      // tab-private sessions are addressable in `sessions` but stay out of view.
      const listed = session.kind !== "tab";
      return {
        sessions: { ...state.sessions, [id]: session },
        order:
          listed && !state.order.includes(id)
            ? [...state.order, id]
            : state.order,
      };
    }),
  removeSession: (sessionId) =>
    set((state) => {
      if (!state.sessions[sessionId]) return state;
      const sessions = { ...state.sessions };
      delete sessions[sessionId];
      return {
        sessions,
        order: state.order.filter((id) => id !== sessionId),
      };
    }),
}));

/** Stable selector for a single session (used by tree/run-flow). */
export function selectSession(
  state: SessionState,
  sessionId: string | null,
): LiveSession | undefined {
  return sessionId ? state.sessions[sessionId] : undefined;
}
