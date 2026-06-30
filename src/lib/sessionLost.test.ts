/**
 * `session:lost` handling: when the backend auto-closes a dropped connection,
 * the matching tab is detached (but remembers its connection for reconnect) and
 * the session is removed from the store.
 */

import { beforeEach, describe, expect, it } from "vitest";

import { handleSessionLost } from "./sessionLost";
import { useEditorStore } from "../state/editorStore";
import { useSessionStore } from "../state/sessionStore";
import type { SessionInfo } from "../ipc/types";

function sessionInfo(sessionId: string): SessionInfo {
  return {
    sessionId,
    driver: "mssql",
    capabilities: {
      schemas: true,
      multiple_result_sets: true,
      server_side_cancel: true,
      transactions: true,
      explain_plan: true,
      streaming_rows: true,
      list_databases: true,
      data_editing: false,
      backup_restore: true,
      database_create_drop: true,
      database_rename: true,
      database_online_offline: true,
    },
  };
}

beforeEach(() => {
  useEditorStore.setState({ tabs: [], activeTabId: null, results: {} });
  useSessionStore.setState({ sessions: {}, order: [] });
  localStorage.clear();
});

describe("handleSessionLost", () => {
  it("detaches the tab but keeps its connection, and removes the session", () => {
    // A tab bound to a private session for connection "c1".
    const tabId = useEditorStore.getState().addTab(null);
    useEditorStore.getState().setTabConnection(tabId, "c1");
    useEditorStore.getState().setTabSession(tabId, "s-tab");
    useSessionStore.getState().addSession({
      info: sessionInfo("s-tab"),
      connectionId: "c1",
      connectionName: "Prod",
      readOnly: false,
      kind: "tab",
    });

    handleSessionLost({ sessionId: "s-tab", connectionId: "c1" });

    const tab = useEditorStore.getState().tabs.find((t) => t.id === tabId);
    expect(tab?.sessionId).toBeNull();
    // Connection is preserved so the toolbar can offer a reconnect.
    expect(tab?.connectionId).toBe("c1");
    expect(useSessionStore.getState().sessions["s-tab"]).toBeUndefined();
  });

  it("removes a browse session (clears it from the sidebar list)", () => {
    useSessionStore.getState().addSession({
      info: sessionInfo("s-browse"),
      connectionId: "c1",
      connectionName: "Prod",
      readOnly: false,
      kind: "browse",
    });
    expect(useSessionStore.getState().order).toContain("s-browse");

    handleSessionLost({ sessionId: "s-browse", connectionId: "c1" });

    expect(useSessionStore.getState().sessions["s-browse"]).toBeUndefined();
    expect(useSessionStore.getState().order).not.toContain("s-browse");
  });

  it("leaves tabs bound to other sessions untouched", () => {
    const tabId = useEditorStore.getState().addTab(null);
    useEditorStore.getState().setTabConnection(tabId, "c2");
    useEditorStore.getState().setTabSession(tabId, "s-other");

    handleSessionLost({ sessionId: "s-dead", connectionId: "c1" });

    const tab = useEditorStore.getState().tabs.find((t) => t.id === tabId);
    expect(tab?.sessionId).toBe("s-other");
  });
});
