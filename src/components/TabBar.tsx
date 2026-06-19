/** Editor tab strip: select, close, and add tabs (+ unsaved/missing markers). */

import { useRef } from "react";

import { requestTabClose, saveTab } from "../lib/fileActions";
import { bindTabConnection, closeTabAndSession } from "../lib/tabSession";
import { useEditorStore } from "../state/editorStore";
import { useSessionStore } from "../state/sessionStore";
import { AddIcon, CloseIcon } from "./icons";
import { Modal } from "./Modal";
import styles from "./TabBar.module.css";

export function TabBar() {
  const tabs = useEditorStore((s) => s.tabs);
  const activeTabId = useEditorStore((s) => s.activeTabId);
  const setActiveTab = useEditorStore((s) => s.setActiveTab);
  const addTab = useEditorStore((s) => s.addTab);
  const pendingCloseTabId = useEditorStore((s) => s.pendingCloseTabId);
  const setPendingCloseTabId = useEditorStore((s) => s.setPendingCloseTabId);
  const sessions = useSessionStore((s) => s.sessions);
  const sessionOrder = useSessionStore((s) => s.order);

  // Open a new tab, cloning a private session for the most-recently-connected
  // connection (if any) so it's ready to run without leaking another tab's db.
  function addQueryTab() {
    const id = addTab(null);
    const lastBrowse = sessionOrder[sessionOrder.length - 1];
    const connectionId = lastBrowse ? sessions[lastBrowse]?.connectionId : null;
    if (connectionId) void bindTabConnection(id, connectionId);
  }

  // Pending close-confirmation for a dirty file-backed tab (don't lose edits).
  const confirmTab = tabs.find((t) => t.id === pendingCloseTabId) ?? null;
  // Retain the title so the Modal can animate out after `pendingCloseTabId` clears.
  const lastTitle = useRef("");
  if (confirmTab) lastTitle.current = confirmTab.title;

  return (
    <div className={styles.bar} role="tablist" aria-label="Editor tabs">
      {tabs.map((tab) => {
        const dirty = tab.filePath !== null && tab.sql !== tab.savedSql;
        return (
          <div
            key={tab.id}
            role="tab"
            aria-selected={tab.id === activeTabId}
            className={`${styles.tab} ${
              tab.id === activeTabId ? styles.active : ""
            } ${tab.fileMissing ? styles.missing : ""}`}
            onClick={() => setActiveTab(tab.id)}
            onAuxClick={(e) => {
              // Middle-click closes.
              if (e.button === 1) requestTabClose(tab.id);
            }}
          >
            <span
              className={styles.title}
              title={
                tab.fileMissing
                  ? `${tab.title} — file no longer on disk`
                  : (tab.filePath ?? tab.title)
              }
            >
              {tab.title}
            </span>
            <span className={styles.tabEnd}>
              {dirty && (
                <span
                  className={styles.dirtyDot}
                  title="Unsaved changes"
                  aria-hidden
                />
              )}
              <button
                type="button"
                className={styles.close}
                aria-label={
                  dirty
                    ? `Close ${tab.title} (unsaved changes)`
                    : `Close ${tab.title}`
                }
                onClick={(e) => {
                  e.stopPropagation();
                  requestTabClose(tab.id);
                }}
              >
                <CloseIcon />
              </button>
            </span>
          </div>
        );
      })}
      <button
        type="button"
        className={styles.add}
        title="New query tab"
        aria-label="New query tab"
        onClick={addQueryTab}
      >
        <AddIcon />
      </button>

      <Modal
        open={pendingCloseTabId !== null}
        title="Unsaved changes"
        tone="warning"
        onClose={() => setPendingCloseTabId(null)}
        footer={
          <>
            <button type="button" onClick={() => setPendingCloseTabId(null)}>
              Cancel
            </button>
            <button
              type="button"
              className="danger"
              onClick={() => {
                if (pendingCloseTabId) closeTabAndSession(pendingCloseTabId);
                setPendingCloseTabId(null);
              }}
            >
              Discard
            </button>
            <button
              type="button"
              className="primary"
              onClick={() => {
                const id = pendingCloseTabId;
                setPendingCloseTabId(null);
                if (id) void saveTab(id).then(() => closeTabAndSession(id));
              }}
            >
              Save
            </button>
          </>
        }
      >
        <p>
          <strong>{confirmTab?.title ?? lastTitle.current}</strong> has unsaved
          changes. Save before closing?
        </p>
      </Modal>
    </div>
  );
}
