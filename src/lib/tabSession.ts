/**
 * Per-tab session lifecycle.
 *
 * A connection and its selected database are unique to each query tab: rather
 * than several tabs sharing one live session (where a `USE [db]` in one tab
 * would change the database for the others on the same physical connection),
 * every tab gets its **own** private session cloned from the chosen connection.
 *
 * This relies on the sidebar having connected first: the connection's browse
 * session supplies the display name + read-only flag, and the in-process secret
 * cache means cloning a session for an already-connected connection does not
 * trigger another keychain prompt.
 */

import {
  connectionsList,
  multiTargetCancel,
  sessionDisconnect,
  sessionUseDatabase,
} from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import type { ConnectionSpec } from "../ipc/types";
import { getTab, useEditorStore } from "../state/editorStore";
import { useMultiTargetStore } from "../state/multiTargetStore";
import { useSessionStore } from "../state/sessionStore";
import { useSettingsStore } from "../state/settingsStore";
import { toastError } from "../state/toastStore";
import { connectSession } from "./connect";
import { matchConnectionForFile } from "./connectionMatch";
import { basename } from "./path";
import { queryClient } from "./queryClient";
import { qk } from "./queries";

/**
 * After (re)connecting a tab, restore the database it was last using so running
 * a query after a dropped link lands back in the same context rather than the
 * connection's default. Best-effort: a database that no longer exists (or any
 * USE failure) just leaves the new session on its default.
 */
async function restoreLastDatabase(
  tabId: string,
  sessionId: string,
): Promise<void> {
  const lastDb = getTab(tabId)?.lastDatabase;
  if (!lastDb) return;
  try {
    await sessionUseDatabase(sessionId, lastDb);
    // The tab may have been closed/rebound while the USE was in flight.
    if (getTab(tabId)?.sessionId === sessionId) {
      useEditorStore.getState().setTabDatabase(tabId, lastDb);
    }
  } catch {
    // Stay on the connection's default database.
  }
}

/** Disconnect and forget a tab-private session. Never touches browse sessions. */
function disposePrivateSession(sessionId: string | null | undefined): void {
  if (!sessionId) return;
  const sess = useSessionStore.getState().sessions[sessionId];
  if (sess?.kind !== "tab") return; // shared/browse session — leave it alone
  void sessionDisconnect(sessionId).catch(() => undefined);
  useSessionStore.getState().removeSession(sessionId);
}

/** The browse (sidebar) session for a connection, if one is open. */
function browseSessionFor(connectionId: string) {
  return Object.values(useSessionStore.getState().sessions).find(
    (s) => s.kind !== "tab" && s.connectionId === connectionId,
  );
}

/** Look up a connection's spec from the TanStack Query cache. */
function connectionSpec(connectionId: string): ConnectionSpec | undefined {
  return queryClient
    .getQueryData<ConnectionSpec[]>(qk.connections())
    ?.find((s) => s.id === connectionId);
}

/**
 * The display name + read-only flag for a connection. Falls back to the cached
 * spec when no browse session is open yet (supports pre-connect use).
 */
function connectionMeta(connectionId: string): {
  name: string;
  readOnly: boolean;
} {
  const browse = browseSessionFor(connectionId);
  if (browse) return { name: browse.connectionName, readOnly: browse.readOnly };
  const spec = connectionSpec(connectionId);
  return {
    name: spec?.name ?? connectionId,
    readOnly: spec?.read_only ?? false,
  };
}

/**
 * Ensure a browse session exists for `connectionId`. If one is already open,
 * this is a no-op. Otherwise it connects and registers the session, handling
 * the race where a concurrent caller wins first (the duplicate is disconnected).
 */
async function ensureBrowseSession(connectionId: string): Promise<void> {
  if (browseSessionFor(connectionId)) return;

  const spec = connectionSpec(connectionId);
  const info = await connectSession(connectionId, spec);
  if (!info) return; // user cancelled the password prompt

  // A concurrent call may have won the race — if so, drop the extra session.
  if (browseSessionFor(connectionId)) {
    void sessionDisconnect(info.sessionId).catch(() => undefined);
    return;
  }

  useSessionStore.getState().addSession({
    info,
    connectionId,
    connectionName: spec?.name ?? connectionId,
    readOnly: spec?.read_only ?? false,
    kind: "browse",
  });
}

/**
 * Connect `tabId` to `connectionId`, opening a browse session first if needed.
 * Pass `null` (or an empty string) to detach the tab from any connection.
 *
 * Unlike `bindTabConnection`, this works even when the connection has never
 * been opened from the sidebar — it calls `ensureBrowseSession` automatically.
 */
export async function connectTabToConnection(
  tabId: string,
  connectionId: string | null,
): Promise<void> {
  if (!connectionId) {
    await bindTabConnection(tabId, null);
    return;
  }
  try {
    await ensureBrowseSession(connectionId);
  } catch (err) {
    toastError("Could not connect", asIpcError(err).message);
    return;
  }
  await bindTabConnection(tabId, connectionId);
}

/**
 * After a file tab opens, connect it to the connection whose name appears in the
 * file name — e.g. opening `pr02db02b_shared_01.sql` connects to a connection
 * named `pr02db02b` (see {@link matchConnectionForFile} for the rules).
 *
 * Best-effort and silent on the no-op paths: it does nothing when the feature is
 * disabled, the tab has no backing file, the tab is *already* connected (an
 * existing connection is never overridden), no connection name matches, or the
 * connection list can't be read. An actual connect failure still surfaces its
 * own toast via {@link connectTabToConnection}. Reuses the normal connect flow,
 * so a connection with no stored password prompts for one just like a sidebar
 * click would.
 */
export async function autoConnectTabFromFile(tabId: string): Promise<void> {
  if (!useSettingsStore.getState().query.autoConnectFromFile) return;

  const tab = getTab(tabId);
  if (!tab?.filePath || tab.sessionId) return;

  // Make sure the connection list is loaded (fetch once if the cache is cold,
  // e.g. opening a file before the sidebar ever queried it).
  let connections: ConnectionSpec[];
  try {
    connections = await queryClient.ensureQueryData({
      queryKey: qk.connections(),
      queryFn: connectionsList,
    });
  } catch {
    return; // can't list connections — nothing to match against
  }

  const connectionId = matchConnectionForFile(
    basename(tab.filePath),
    connections,
  );
  if (!connectionId) return;

  // The tab may have been bound or closed while we awaited the list — re-check
  // so we never override a connection the user (or a race) set meanwhile.
  const current = getTab(tabId);
  if (!current || current.sessionId) return;

  await connectTabToConnection(tabId, connectionId);
}

/** Whether any open tab still holds a private session for `connectionId`. */
function connectionHasTabs(connectionId: string): boolean {
  const { sessions } = useSessionStore.getState();
  return useEditorStore.getState().tabs.some((tab) => {
    const sess = tab.sessionId ? sessions[tab.sessionId] : undefined;
    return sess?.kind === "tab" && sess.connectionId === connectionId;
  });
}

/**
 * Reference-counted browse-session cleanup: once no open tab is using
 * `connectionId`, disconnect its sidebar browse session and remove it from the
 * schema tree. Tabs sharing a connection keep it alive until the last closes.
 */
function disconnectBrowseIfUnused(connectionId: string): void {
  if (connectionHasTabs(connectionId)) return;
  const browse = browseSessionFor(connectionId);
  if (!browse) return;
  void sessionDisconnect(browse.info.sessionId).catch(() => undefined);
  useSessionStore.getState().removeSession(browse.info.sessionId);
}

/**
 * Bind `tabId` to its own private session for `connectionId`, disposing any
 * prior private session for the tab first. Pass `null` to detach the tab from
 * any connection. Resolves once the (best-effort) clone has completed.
 */
export async function bindTabConnection(
  tabId: string,
  connectionId: string | null,
): Promise<void> {
  const prev = getTab(tabId)?.sessionId ?? null;
  disposePrivateSession(prev);
  useEditorStore.getState().setTabSession(tabId, null);

  if (!connectionId) {
    // Explicit detach — forget the intended connection too.
    useEditorStore.getState().setTabConnection(tabId, null);
    return;
  }

  // Record the intended connection up front so the toolbar reflects it while we
  // connect, and so it survives a later session drop (for the reconnect button).
  useEditorStore.getState().setTabConnection(tabId, connectionId);

  const { name, readOnly } = connectionMeta(connectionId);
  try {
    const info = await connectSession(
      connectionId,
      connectionSpec(connectionId),
    );
    if (!info) return; // user cancelled the password prompt
    useSessionStore.getState().addSession({
      info,
      connectionId,
      connectionName: name,
      readOnly,
      kind: "tab",
    });
    // The tab may have been closed or rebound while we awaited the connect.
    if (getTab(tabId) && !getTab(tabId)?.sessionId) {
      useEditorStore.getState().setTabSession(tabId, info.sessionId);
      // Land back in the database the tab last used (e.g. after a USE), so a
      // reconnect to the same connection doesn't silently switch context to the
      // server default. No-op on a first connect / connection switch.
      await restoreLastDatabase(tabId, info.sessionId);
    } else {
      disposePrivateSession(info.sessionId); // orphan from a racing rebind
    }
  } catch (err) {
    toastError("Could not open connection", asIpcError(err).message);
  }
}

/**
 * Dispose every tab-private session opened from `connectionId` and detach the
 * tabs that used them. Called when the connection is disconnected from the
 * sidebar so its tabs don't keep a now-orphaned (and invisible) session.
 */
export function disposeConnectionTabs(connectionId: string): void {
  const { sessions } = useSessionStore.getState();
  for (const tab of useEditorStore.getState().tabs) {
    const sess = tab.sessionId ? sessions[tab.sessionId] : undefined;
    if (sess?.kind === "tab" && sess.connectionId === connectionId) {
      disposePrivateSession(tab.sessionId);
      useEditorStore.getState().setTabSession(tab.id, null);
    }
  }
}

/**
 * Close a tab and dispose of its private session. If it was the last tab using
 * that connection, also disconnect the sidebar's browse session (reference
 * counted) so the connection fully closes and leaves the schema tree.
 */
export function closeTabAndSession(tabId: string): void {
  // A multi-target tab has no session/file, but may have an in-flight run and a
  // stored view: cancel the run and drop the view so nothing leaks.
  if (getTab(tabId)?.kind === "multiTarget") {
    const view = useMultiTargetStore.getState().views[tabId];
    if (view?.runId) void multiTargetCancel(view.runId).catch(() => undefined);
    useMultiTargetStore.getState().remove(tabId);
    useEditorStore.getState().closeTab(tabId);
    return;
  }
  const sessionId = getTab(tabId)?.sessionId;
  const connectionId = sessionId
    ? useSessionStore.getState().sessions[sessionId]?.connectionId
    : undefined;
  disposePrivateSession(sessionId);
  useEditorStore.getState().closeTab(tabId);
  if (connectionId) disconnectBrowseIfUnused(connectionId);
}
