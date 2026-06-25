/**
 * Left rail: saved connections (connect / edit / delete) above the schema tree
 * for whichever session is active.
 *
 * The three panels (Connections, Files, Schema) are:
 *   - Resizable  — drag the divider between any two panels
 *   - Collapsible — click the caret in the section header
 *   - Reorderable — drag the grip icon in the section header
 *
 * Layout state (width, order, heights, collapsed) is persisted in layoutStore.
 */

import { Fragment, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";

import {
  connectionDelete,
  connectionReorder,
  sessionDisconnect,
} from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import type { ConnectionSpec } from "../ipc/types";
import { connectSession } from "../lib/connect";
import { openFileDialog, openFolderDialog } from "../lib/fileActions";
import { matches } from "../lib/filterMatch";
import { qk, useConnections } from "../lib/queries";
import { useTypeToFilter } from "../lib/useTypeToFilter";
import { bindTabConnection, disposeConnectionTabs } from "../lib/tabSession";
import { useEditorStore } from "../state/editorStore";
import { type PanelId, useLayoutStore } from "../state/layoutStore";
import { useSessionStore } from "../state/sessionStore";
import { toastError } from "../state/toastStore";
import { useWorkspaceStore } from "../state/workspaceStore";
import { ConnectionDialog } from "./ConnectionDialog";
import { FileTree } from "./FileTree";
import { FilterIndicator } from "./FilterIndicator";
import {
  AddIcon,
  CaretIcon,
  ConnectionIcon,
  DeleteIcon,
  DragHandleIcon,
  EditIcon,
  FileIcon,
  FolderIcon,
  MultiTargetIcon,
  PanelGripIcon,
} from "./icons";
import { SchemaTree } from "./SchemaTree";
import styles from "./Sidebar.module.css";

const PANEL_TITLES: Record<PanelId, string> = {
  connections: "Connections",
  files: "Files",
  schema: "Schema",
};

export function Sidebar() {
  const queryClient = useQueryClient();
  const { data: connections = [], isLoading } = useConnections();

  const sessions = useSessionStore((s) => s.sessions);
  const sessionOrder = useSessionStore((s) => s.order);
  const addSession = useSessionStore((s) => s.addSession);
  const removeSession = useSessionStore((s) => s.removeSession);

  const openFolders = useWorkspaceStore((s) => s.openFolders);

  const [dialogOpen, setDialogOpen] = useState(false);
  const [editing, setEditing] = useState<ConnectionSpec | null>(null);
  const [connectingId, setConnectingId] = useState<string | null>(null);

  // Layout store
  const sidebarWidth = useLayoutStore((s) => s.sidebarWidth);
  const panelOrder = useLayoutStore((s) => s.panelOrder);
  const collapsed = useLayoutStore((s) => s.collapsed);
  const panelH = useLayoutStore((s) => s.panelH);
  const toggleCollapsed = useLayoutStore((s) => s.toggleCollapsed);
  const setPanelOrder = useLayoutStore((s) => s.setPanelOrder);
  const setPanelH = useLayoutStore((s) => s.setPanelH);

  // ── Type-to-filter (no visible box; type while a panel has focus) ──────────
  const connFilter = useTypeToFilter();
  const filesFilter = useTypeToFilter();
  const schemaFilter = useTypeToFilter();
  const filters: Record<PanelId, ReturnType<typeof useTypeToFilter>> = {
    connections: connFilter,
    files: filesFilter,
    schema: schemaFilter,
  };

  // ── Connection drag-to-reorder ─────────────────────────────────────────────
  const dragIndex = useRef<number | null>(null);
  const [overIndex, setOverIndex] = useState<number | null>(null);

  // ── Panel drag-to-reorder ──────────────────────────────────────────────────
  const panelDragFrom = useRef<number | null>(null);
  const [panelDragOver, setPanelDragOver] = useState<number | null>(null);

  // ── Connection actions ─────────────────────────────────────────────────────

  function openNew() {
    setEditing(null);
    setDialogOpen(true);
  }

  function openEdit(spec: ConnectionSpec) {
    setEditing(spec);
    setDialogOpen(true);
  }

  async function doConnect(spec: ConnectionSpec) {
    setConnectingId(spec.id);
    try {
      // Prompts inline (and retries) if no password is stored; null = cancelled.
      const info = await connectSession(spec.id, spec);
      if (!info) return;
      addSession({
        info,
        connectionId: spec.id,
        connectionName: spec.name,
        readOnly: spec.read_only,
        kind: "browse",
      });
      const ed = useEditorStore.getState();
      const active = ed.tabs.find((t) => t.id === ed.activeTabId) ?? null;
      const reusable =
        active !== null &&
        active.filePath === null &&
        active.sessionId === null &&
        active.sql.trim() === "";
      const targetTab = reusable ? active.id : ed.addTab(null);
      await bindTabConnection(targetTab, spec.id);
    } catch (e) {
      const ipc = asIpcError(e);
      toastError(`Could not connect to "${spec.name}"`, ipc.message);
    } finally {
      setConnectingId(null);
    }
  }

  async function doDelete(spec: ConnectionSpec) {
    try {
      await connectionDelete(spec.id);
      await queryClient.invalidateQueries({ queryKey: qk.connections() });
    } catch (e) {
      const ipc = asIpcError(e);
      toastError("Could not delete connection", ipc.message);
    }
  }

  function disconnect(sessionId: string) {
    const connectionId = sessions[sessionId]?.connectionId;
    void sessionDisconnect(sessionId).catch(() => undefined);
    removeSession(sessionId);
    if (connectionId) disposeConnectionTabs(connectionId);
  }

  // Connection item drag handlers — guard against accidental activation during
  // a panel drag (panelDragFrom.current !== null).
  function handleConnDragStart(index: number) {
    dragIndex.current = index;
  }

  function handleConnDragEnter(e: React.DragEvent, index: number) {
    if (panelDragFrom.current !== null) return;
    e.preventDefault();
    setOverIndex(index);
  }

  function handleConnDragOver(e: React.DragEvent) {
    if (panelDragFrom.current !== null) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
  }

  function handleConnDragLeave() {
    if (panelDragFrom.current !== null) return;
    setOverIndex(null);
  }

  async function handleConnDrop(e: React.DragEvent, toIndex: number) {
    e.preventDefault();
    e.stopPropagation(); // prevent the section's onDrop from firing
    if (panelDragFrom.current !== null) return;
    const fromIndex = dragIndex.current;
    dragIndex.current = null;
    setOverIndex(null);
    if (fromIndex === null || fromIndex === toIndex) return;

    const reordered = [...connections];
    const [moved] = reordered.splice(fromIndex, 1);
    reordered.splice(toIndex, 0, moved);
    const ids = reordered.map((s) => s.id);

    queryClient.setQueryData<ConnectionSpec[]>(qk.connections(), reordered);
    try {
      await connectionReorder(ids);
      await queryClient.invalidateQueries({ queryKey: qk.connections() });
    } catch (e) {
      const ipc = asIpcError(e);
      toastError("Could not reorder connections", ipc.message);
      await queryClient.invalidateQueries({ queryKey: qk.connections() });
    }
  }

  function handleConnDragEnd() {
    dragIndex.current = null;
    setOverIndex(null);
  }

  // ── Panel height resize ────────────────────────────────────────────────────

  function onPanelResizerDown(e: React.MouseEvent, panelId: PanelId) {
    e.preventDefault();
    const startY = e.clientY;
    const startH = panelH[panelId];
    function onMove(ev: MouseEvent) {
      setPanelH(panelId, startH + (ev.clientY - startY));
    }
    function onUp() {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      document.body.style.cursor = "";
    }
    document.body.style.cursor = "row-resize";
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }

  // ── Helpers ────────────────────────────────────────────────────────────────

  const connectedIds = new Set(
    Object.values(sessions).map((s) => s.connectionId),
  );

  function renderHeaderActions(panelId: PanelId) {
    switch (panelId) {
      case "connections":
        return (
          <div className={styles.headerActions}>
            <button
              type="button"
              className="ghost"
              title="Run on multiple targets"
              aria-label="Run on multiple targets"
              onClick={() => useEditorStore.getState().addMultiTargetTab()}
            >
              <MultiTargetIcon />
            </button>
            <button
              type="button"
              className="ghost"
              title="New connection"
              aria-label="New connection"
              onClick={openNew}
            >
              <AddIcon />
            </button>
          </div>
        );
      case "files":
        return (
          <div className={styles.headerActions}>
            <button
              type="button"
              className="ghost"
              title="Open folder"
              aria-label="Open folder"
              onClick={() => void openFolderDialog()}
            >
              <FolderIcon />
            </button>
            <button
              type="button"
              className="ghost"
              title="Open SQL file"
              aria-label="Open SQL file"
              onClick={() => void openFileDialog()}
            >
              <FileIcon />
            </button>
          </div>
        );
      case "schema":
        return null;
    }
  }

  function renderPanelContent(panelId: PanelId) {
    switch (panelId) {
      case "connections": {
        const visible = connections.filter((s) =>
          matches(s.name, connFilter.query),
        );
        return isLoading ? (
          <div className={`${styles.placeholder} ${styles.loading}`}>
            <span className="spinner" aria-hidden /> Loading…
          </div>
        ) : connections.length === 0 ? (
          <div className={styles.placeholder}>
            No connections yet.
            <button type="button" className={styles.linkBtn} onClick={openNew}>
              Create one
            </button>
          </div>
        ) : visible.length === 0 ? (
          <div className={styles.placeholder}>No matches.</div>
        ) : (
          <ul className={styles.connList}>
            {visible.map((spec) => {
              // Drag uses the index into the full list so reordering stays
              // correct even while a filter hides some rows.
              const idx = connections.indexOf(spec);
              const isConnected = connectedIds.has(spec.id);
              const isDraggingItem = dragIndex.current === idx;
              const isOverItem = overIndex === idx;
              return (
                <li
                  key={spec.id}
                  className={[
                    styles.connItem,
                    isDraggingItem ? styles.connItemDragging : "",
                    isOverItem ? styles.connItemOver : "",
                  ]
                    .filter(Boolean)
                    .join(" ")}
                  draggable
                  onDragStart={() => handleConnDragStart(idx)}
                  onDragEnter={(e) => handleConnDragEnter(e, idx)}
                  onDragOver={handleConnDragOver}
                  onDragLeave={handleConnDragLeave}
                  onDrop={(e) => void handleConnDrop(e, idx)}
                  onDragEnd={handleConnDragEnd}
                >
                  <span className={styles.dragHandle} aria-hidden>
                    <DragHandleIcon />
                  </span>
                  <button
                    type="button"
                    className={styles.connMain}
                    title={`${spec.host}${spec.port ? `:${spec.port}` : ""}`}
                    onClick={() => doConnect(spec)}
                    disabled={connectingId === spec.id}
                  >
                    <span
                      className={`${styles.dot} ${
                        connectingId === spec.id
                          ? styles.dotConnecting
                          : isConnected
                            ? styles.dotOn
                            : ""
                      }`}
                      aria-hidden
                    />
                    <ConnectionIcon className={styles.connIcon} />
                    <span className={styles.connName}>{spec.name}</span>
                    {spec.read_only && (
                      <span className={styles.roBadge} title="Read-only">
                        RO
                      </span>
                    )}
                  </button>
                  <div className={styles.connActions}>
                    <button
                      type="button"
                      className="ghost"
                      title="Edit"
                      aria-label={`Edit ${spec.name}`}
                      onClick={() => openEdit(spec)}
                    >
                      <EditIcon />
                    </button>
                    <button
                      type="button"
                      className="ghost"
                      title="Delete"
                      aria-label={`Delete ${spec.name}`}
                      onClick={() => doDelete(spec)}
                    >
                      <DeleteIcon />
                    </button>
                  </div>
                </li>
              );
            })}
          </ul>
        );
      }

      case "files":
        return openFolders.length === 0 ? (
          <div className={styles.placeholder}>
            No folders open.
            <button
              type="button"
              className={styles.linkBtn}
              onClick={() => void openFolderDialog()}
            >
              Open a folder
            </button>
          </div>
        ) : (
          <div className={styles.filesScroll}>
            {openFolders.map((folder) => (
              <FileTree
                key={folder}
                folder={folder}
                filter={filesFilter.query}
              />
            ))}
          </div>
        );

      case "schema":
        return (
          <div className={styles.treeScroll}>
            {sessionOrder.length === 0 ? (
              <div className={styles.placeholder}>
                Connect to browse databases.
              </div>
            ) : (
              sessionOrder.map((sid) => {
                const session = sessions[sid];
                if (!session) return null;
                return (
                  <SchemaTree
                    key={sid}
                    session={session}
                    onDisconnect={() => disconnect(sid)}
                    filter={schemaFilter.query}
                  />
                );
              })
            )}
          </div>
        );
    }
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  return (
    <aside
      className={styles.sidebar}
      style={{ width: sidebarWidth, minWidth: sidebarWidth }}
    >
      {panelOrder.map((panelId, idx) => {
        const isLast = idx === panelOrder.length - 1;
        const isCollapsed = collapsed[panelId];

        // Last panel grows to fill; others get their stored height as a *flex
        // basis* (not a fixed height) so they can shrink when the combined panel
        // heights would overflow the sidebar. Each section keeps a min-height of
        // its header (see .section in the CSS), so title bars stay visible and the
        // inner lists scroll within whatever space remains.
        const sectionStyle: React.CSSProperties = isLast
          ? {}
          : isCollapsed
            ? {}
            : { flexBasis: panelH[panelId] };

        const isDragOver = panelDragOver === idx;

        return (
          <Fragment key={panelId}>
            <div
              className={[
                styles.section,
                isLast ? styles.sectionFlex : "",
                isDragOver ? styles.sectionOver : "",
              ]
                .filter(Boolean)
                .join(" ")}
              style={sectionStyle}
              // Panel-level drop target for reordering.
              onDragEnter={(e) => {
                if (panelDragFrom.current === null) return;
                e.preventDefault();
                setPanelDragOver(idx);
              }}
              onDragOver={(e) => {
                if (panelDragFrom.current === null) return;
                e.preventDefault();
                e.dataTransfer.dropEffect = "move";
              }}
              onDragLeave={(e) => {
                // Only clear when truly leaving the section, not entering a child.
                const related = e.relatedTarget as Node | null;
                if (related && (e.currentTarget as Node).contains(related))
                  return;
                setPanelDragOver(null);
              }}
              onDrop={(e) => {
                const fromIdx = panelDragFrom.current;
                if (fromIdx === null) return;
                e.preventDefault();
                panelDragFrom.current = null;
                setPanelDragOver(null);
                if (fromIdx === idx) return;
                const order = [...panelOrder];
                const [moved] = order.splice(fromIdx, 1);
                order.splice(idx, 0, moved);
                setPanelOrder(order);
              }}
            >
              {/* ── Section header ──────────────────────────────────── */}
              <div className={styles.sectionHeader}>
                {/* Grip for panel reordering */}
                <span
                  className={styles.panelGrip}
                  draggable
                  onDragStart={(e) => {
                    e.stopPropagation();
                    panelDragFrom.current = idx;
                  }}
                  onDragEnd={() => {
                    panelDragFrom.current = null;
                    setPanelDragOver(null);
                  }}
                  title="Drag to reorder panel"
                  aria-hidden
                >
                  <PanelGripIcon />
                </span>

                {/* Collapse toggle */}
                <button
                  type="button"
                  className={styles.collapseToggle}
                  onClick={() => toggleCollapsed(panelId)}
                  aria-expanded={!isCollapsed}
                  aria-label={`${isCollapsed ? "Expand" : "Collapse"} ${PANEL_TITLES[panelId]}`}
                >
                  <span
                    className={styles.caretWrap}
                    data-open={!isCollapsed ? "true" : undefined}
                  >
                    <CaretIcon />
                  </span>
                </button>

                <span className={styles.panelTitle}>
                  {PANEL_TITLES[panelId]}
                </span>

                {renderHeaderActions(panelId)}
              </div>

              {/* ── Section body (animated collapse) ────────────────── */}
              <div
                className={[
                  styles.sectionBody,
                  isCollapsed ? styles.sectionBodyCollapsed : "",
                ]
                  .filter(Boolean)
                  .join(" ")}
              >
                {/* tabIndex lets a click on empty panel space focus the
                    wrapper, so type-to-filter works without first focusing a
                    row. Keystrokes bubble here from focused rows/buttons. */}
                <div
                  className={styles.sectionBodyInner}
                  style={{ position: "relative" }}
                  tabIndex={-1}
                  onKeyDown={filters[panelId].onKeyDown}
                >
                  <FilterIndicator query={filters[panelId].query} />
                  {renderPanelContent(panelId)}
                </div>
              </div>
            </div>

            {/* Resize handle between this panel and the next */}
            {!isLast && (
              <div
                className={styles.panelResizer}
                onMouseDown={(e) => onPanelResizerDown(e, panelId)}
                aria-hidden
              />
            )}
          </Fragment>
        );
      })}

      <ConnectionDialog
        open={dialogOpen}
        initial={editing}
        onClose={() => setDialogOpen(false)}
        onSaved={async () => {
          setDialogOpen(false);
          await queryClient.invalidateQueries({ queryKey: qk.connections() });
        }}
        onSaveAndConnect={async (spec) => {
          setDialogOpen(false);
          await queryClient.invalidateQueries({ queryKey: qk.connections() });
          await doConnect(spec);
        }}
      />
    </aside>
  );
}
