/**
 * Lazy schema browser for one live session.
 *
 * Each level fetches on first expand via TanStack Query (database -> schemas ->
 * tables/views -> columns), so nothing is loaded until the user drills in, and
 * re-expanding is instant (cached). Double-clicking a table inserts a
 * `SELECT TOP 100 *` into the active editor tab. The "schemas" level is gated on
 * the driver capability flag (skipped if the driver has no schema concept).
 */

import { createContext, useCallback, useContext, useState } from "react";
import type { ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";

import {
  sessionCreateDatabase,
  sessionDropDatabase,
  sessionRenameDatabase,
  sessionSetDatabaseOnline,
} from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import type {
  ColumnInfo,
  DatabaseInfo,
  SchemaInfo,
  TableInfo,
} from "../ipc/types";
import { matches } from "../lib/filterMatch";
import {
  qk,
  useColumns,
  useDatabases,
  useSchemas,
  useTables,
} from "../lib/queries";
import { useEditorStore } from "../state/editorStore";
import { useImportStore } from "../state/importStore";
import type { LiveSession } from "../state/sessionStore";
import { toastError } from "../state/toastStore";
import { ContextMenu, type ContextMenuItem } from "./ContextMenu";
import { Modal } from "./Modal";
import {
  CaretIcon,
  ColumnIcon,
  ConnectionIcon,
  DatabaseIcon,
  DisconnectIcon,
  PrimaryKeyIcon,
  SchemaIcon,
  TableIcon,
  ViewIcon,
} from "./icons";
import styles from "./SchemaTree.module.css";

interface SchemaTreeProps {
  session: LiveSession;
  onDisconnect: () => void;
  /** Type-to-filter query; nodes self-hide unless their name matches (or they
   *  are expanded, so drilling in is never undone by typing). */
  filter?: string;
}

/** A destructive-action confirmation request raised by a tree node. */
interface ConfirmRequest {
  title: string;
  message: string;
  confirmLabel: string;
  onConfirm: () => void;
  /** When set, the user must re-type this exact text before the confirm button
   *  enables (guards an irreversible action like dropping a database). */
  requireText?: string;
}

/** Active type-to-filter query, threaded to every level without prop drilling. */
const FilterContext = createContext<string>("");

/** Opens the tree's shared confirm dialog (for destructive database actions). */
const ConfirmContext = createContext<(req: ConfirmRequest) => void>(
  () => undefined,
);

/** Quote a SQL identifier for MSSQL (brackets, escape `]`). */
function quoteIdent(name: string): string {
  return `[${name.replace(/]/g, "]]")}]`;
}

/**
 * Opens the tree's shared context menu at the cursor with the given items.
 * Provided by `SchemaTree` so deeply-nested nodes can raise a right-click menu
 * without threading a callback through every level.
 */
const MenuContext = createContext<
  (e: React.MouseEvent, items: ContextMenuItem[]) => void
>(() => undefined);

function Row({
  depth,
  expandable,
  expanded,
  onToggle,
  onActivate,
  onDoubleClick,
  onContextMenu,
  icon,
  label,
  badge,
  title,
  dim,
}: {
  depth: number;
  expandable: boolean;
  expanded?: boolean;
  onToggle?: () => void;
  onActivate?: () => void;
  onDoubleClick?: () => void;
  onContextMenu?: (e: React.MouseEvent) => void;
  icon: ReactNode;
  label: string;
  badge?: string;
  title?: string;
  /** Render the row muted (e.g. an offline database). */
  dim?: boolean;
}) {
  const activate = onActivate ?? onToggle;
  return (
    <div
      className={styles.row}
      style={{
        paddingLeft: depth * 14 + 6,
        ...(dim ? { color: "var(--text-faint)" } : null),
      }}
      role="treeitem"
      aria-expanded={expandable ? !!expanded : undefined}
      tabIndex={0}
      title={title ?? label}
      onClick={activate}
      onDoubleClick={onDoubleClick}
      onContextMenu={onContextMenu}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          activate?.();
        }
      }}
    >
      {expandable ? (
        <span
          className={`${styles.caret} ${expanded ? styles.caretOpen : ""}`}
          onClick={(e) => {
            e.stopPropagation();
            onToggle?.();
          }}
          aria-hidden
        >
          <CaretIcon />
        </span>
      ) : (
        <span className={styles.caretSpacer} aria-hidden />
      )}
      <span className={styles.icon} aria-hidden>
        {icon}
      </span>
      <span className={styles.label}>{label}</span>
      {badge && <span className={styles.badge}>{badge}</span>}
    </div>
  );
}

function ColumnsLevel({
  sessionId,
  database,
  schema,
  table,
  depth,
}: {
  sessionId: string;
  database: string;
  schema: string;
  table: string;
  depth: number;
}) {
  const query = useContext(FilterContext);
  const { data, isLoading, error } = useColumns(
    sessionId,
    database,
    schema,
    table,
    true,
  );
  if (isLoading) return <Loading depth={depth} />;
  if (error) return <ErrorRow depth={depth} />;
  return (
    <>
      {(data ?? [])
        .filter((col) => matches(col.name, query))
        .map((col: ColumnInfo) => (
          <Row
            key={col.name}
            depth={depth}
            expandable={false}
            icon={col.is_primary_key ? <PrimaryKeyIcon /> : <ColumnIcon />}
            label={col.name}
            badge={col.data_type}
            title={`${col.name} ${col.data_type}${col.nullable ? "" : " NOT NULL"}`}
          />
        ))}
    </>
  );
}

function TableNode({
  sessionId,
  database,
  schema,
  table,
  depth,
}: {
  sessionId: string;
  database: string;
  schema: string;
  table: TableInfo;
  depth: number;
}) {
  const [open, setOpen] = useState(false);
  const appendSql = useEditorStore((s) => s.appendSql);
  const activeTabId = useEditorStore((s) => s.activeTabId);
  const openMenu = useContext(MenuContext);
  const query = useContext(FilterContext);
  const requestImport = useImportStore((s) => s.requestImport);

  // Self-hide unless the name matches or the node is expanded (so drilling in
  // is never undone by typing).
  if (!matches(table.name, query) && !open) return null;

  function insertSelect() {
    if (!activeTabId) return;
    const sql = `SELECT TOP 100 * FROM ${quoteIdent(schema)}.${quoteIdent(
      table.name,
    )};`;
    appendSql(activeTabId, sql);
  }

  return (
    <>
      <Row
        depth={depth}
        expandable
        expanded={open}
        onToggle={() => setOpen((o) => !o)}
        onDoubleClick={insertSelect}
        onContextMenu={
          // Views can't be imported into; only base tables.
          table.kind === "table"
            ? (e) =>
                openMenu(e, [
                  {
                    label: "Import CSV into table…",
                    onSelect: () =>
                      requestImport({
                        sessionId,
                        database,
                        schema,
                        mode: "existing",
                        table: table.name,
                      }),
                  },
                ])
            : undefined
        }
        icon={table.kind === "view" ? <ViewIcon /> : <TableIcon />}
        label={table.name}
        title={`${schema}.${table.name} (${table.kind}) — double-click to query`}
      />
      {open && (
        <ColumnsLevel
          sessionId={sessionId}
          database={database}
          schema={schema}
          table={table.name}
          depth={depth + 1}
        />
      )}
    </>
  );
}

function TablesLevel({
  sessionId,
  database,
  schema,
  depth,
}: {
  sessionId: string;
  database: string;
  schema: string;
  depth: number;
}) {
  const { data, isLoading, error } = useTables(
    sessionId,
    database,
    schema,
    true,
  );
  if (isLoading) return <Loading depth={depth} />;
  if (error) return <ErrorRow depth={depth} />;
  if ((data ?? []).length === 0)
    return <EmptyRow depth={depth} label="No tables" />;
  return (
    <>
      {(data ?? []).map((t) => (
        <TableNode
          key={`${t.schema}.${t.name}`}
          sessionId={sessionId}
          database={database}
          schema={schema}
          table={t}
          depth={depth}
        />
      ))}
    </>
  );
}

function SchemaNode({
  sessionId,
  database,
  schema,
  depth,
}: {
  sessionId: string;
  database: string;
  schema: SchemaInfo;
  depth: number;
}) {
  const [open, setOpen] = useState(false);
  const openMenu = useContext(MenuContext);
  const query = useContext(FilterContext);
  const requestImport = useImportStore((s) => s.requestImport);

  if (!matches(schema.name, query) && !open) return null;

  return (
    <>
      <Row
        depth={depth}
        expandable
        expanded={open}
        onToggle={() => setOpen((o) => !o)}
        onContextMenu={(e) =>
          openMenu(e, [
            {
              label: "Import CSV as new table…",
              onSelect: () =>
                requestImport({
                  sessionId,
                  database,
                  schema: schema.name,
                  mode: "new",
                }),
            },
          ])
        }
        icon={<SchemaIcon />}
        label={schema.name}
      />
      {open && (
        <TablesLevel
          sessionId={sessionId}
          database={database}
          schema={schema.name}
          depth={depth + 1}
        />
      )}
    </>
  );
}

function SchemasLevel({
  sessionId,
  database,
  depth,
}: {
  sessionId: string;
  database: string;
  depth: number;
}) {
  const { data, isLoading, error } = useSchemas(sessionId, database, true);
  if (isLoading) return <Loading depth={depth} />;
  if (error) return <ErrorRow depth={depth} />;
  return (
    <>
      {(data ?? []).map((s) => (
        <SchemaNode
          key={s.name}
          sessionId={sessionId}
          database={database}
          schema={s}
          depth={depth}
        />
      ))}
    </>
  );
}

function DatabaseNode({
  session,
  database,
  depth,
}: {
  session: LiveSession;
  database: DatabaseInfo;
  depth: number;
}) {
  const [open, setOpen] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [draft, setDraft] = useState(database.name);
  const sessionId = session.info.sessionId;
  const hasSchemas = session.info.capabilities.schemas;
  const openMenu = useContext(MenuContext);
  const requestConfirm = useContext(ConfirmContext);
  const query = useContext(FilterContext);
  const queryClient = useQueryClient();

  const isOnline = database.state_desc === "ONLINE";
  // System databases and read-only connections can't be managed.
  const canManage = !database.is_system && !session.readOnly;

  if (!matches(database.name, query) && !open) return null;

  function refresh() {
    return queryClient.invalidateQueries({ queryKey: qk.databases(sessionId) });
  }

  async function commitRename() {
    const next = draft.trim();
    setRenaming(false);
    if (!next || next === database.name) return;
    try {
      await sessionRenameDatabase(sessionId, database.name, next);
      await refresh();
    } catch (e) {
      toastError("Could not rename database", asIpcError(e).message);
    }
  }

  async function setOnline(online: boolean) {
    try {
      await sessionSetDatabaseOnline(sessionId, database.name, online);
      await refresh();
    } catch (e) {
      toastError(
        online
          ? "Could not bring database online"
          : "Could not take database offline",
        asIpcError(e).message,
      );
    }
  }

  async function drop() {
    try {
      await sessionDropDatabase(sessionId, database.name);
      await refresh();
    } catch (e) {
      toastError("Could not drop database", asIpcError(e).message);
    }
  }

  function openContextMenu(e: React.MouseEvent) {
    const items: ContextMenuItem[] = [
      {
        label: "Rename…",
        disabled: !canManage,
        onSelect: () => {
          setDraft(database.name);
          setRenaming(true);
        },
      },
      isOnline
        ? {
            label: "Take offline",
            disabled: !canManage,
            onSelect: () =>
              requestConfirm({
                title: "Take database offline",
                confirmLabel: "Take offline",
                message: `Take "${database.name}" offline? This immediately terminates all connections to it (ROLLBACK IMMEDIATE).`,
                onConfirm: () => void setOnline(false),
              }),
          }
        : {
            label: "Bring online",
            disabled: !canManage,
            onSelect: () => void setOnline(true),
          },
      {
        label: "Drop…",
        disabled: !canManage,
        onSelect: () =>
          requestConfirm({
            title: "Drop database",
            confirmLabel: "Drop",
            message: `Permanently drop "${database.name}"? This deletes the database and all its data. This cannot be undone.`,
            requireText: database.name,
            onConfirm: () => void drop(),
          }),
      },
    ];
    openMenu(e, items);
  }

  return (
    <>
      {renaming ? (
        <div className={styles.row} style={{ paddingLeft: depth * 14 + 6 }}>
          <span className={styles.caretSpacer} aria-hidden />
          <span className={styles.icon} aria-hidden>
            <DatabaseIcon />
          </span>
          <input
            className={styles.renameInput}
            autoFocus
            value={draft}
            aria-label={`Rename database ${database.name}`}
            onChange={(e) => setDraft(e.target.value)}
            onClick={(e) => e.stopPropagation()}
            onKeyDown={(e) => {
              e.stopPropagation();
              if (e.key === "Enter") {
                e.preventDefault();
                void commitRename();
              } else if (e.key === "Escape") {
                e.preventDefault();
                setRenaming(false);
              }
            }}
            onBlur={() => setRenaming(false)}
          />
        </div>
      ) : (
        <Row
          depth={depth}
          // Offline databases can't be browsed; only managed via the menu.
          expandable={isOnline}
          expanded={open}
          onToggle={isOnline ? () => setOpen((o) => !o) : undefined}
          onContextMenu={openContextMenu}
          dim={!isOnline}
          icon={
            database.is_system ? (
              <DatabaseIcon style={{ color: "var(--text-faint)" }} />
            ) : (
              <DatabaseIcon />
            )
          }
          label={database.name}
          badge={isOnline ? undefined : database.state_desc}
          title={
            database.is_system
              ? `${database.name} (system, ${database.state_desc})`
              : `${database.name} (${database.state_desc})`
          }
        />
      )}
      {open &&
        isOnline &&
        (hasSchemas ? (
          <SchemasLevel
            sessionId={sessionId}
            database={database.name}
            depth={depth + 1}
          />
        ) : (
          // Drivers without schemas list tables directly under the database
          // using a synthetic default schema name.
          <TablesLevel
            sessionId={sessionId}
            database={database.name}
            schema="dbo"
            depth={depth + 1}
          />
        ))}
    </>
  );
}

export function SchemaTree({
  session,
  onDisconnect,
  filter = "",
}: SchemaTreeProps) {
  const [open, setOpen] = useState(true);
  const sessionId = session.info.sessionId;
  const { data, isLoading, error } = useDatabases(sessionId, open);

  // One shared context menu per tree, positioned at the cursor. `menu` is
  // retained while it animates closed; `menuOpen` drives the exit.
  const [menu, setMenu] = useState<{
    x: number;
    y: number;
    items: ContextMenuItem[];
  } | null>(null);
  const [menuOpen, setMenuOpen] = useState(false);
  const openMenu = useCallback(
    (e: React.MouseEvent, items: ContextMenuItem[]) => {
      e.preventDefault();
      e.stopPropagation();
      setMenu({ x: e.clientX, y: e.clientY, items });
      setMenuOpen(true);
    },
    [],
  );

  // One shared confirm dialog per tree for destructive database actions.
  const [confirm, setConfirm] = useState<ConfirmRequest | null>(null);
  const [confirmOpen, setConfirmOpen] = useState(false);
  // For type-to-confirm requests: the text the user has typed so far.
  const [confirmInput, setConfirmInput] = useState("");
  const requestConfirm = useCallback((req: ConfirmRequest) => {
    setConfirm(req);
    setConfirmInput("");
    setConfirmOpen(true);
  }, []);
  // Type-to-confirm requests stay disabled until the typed text matches exactly.
  const confirmReady =
    confirm?.requireText == null || confirmInput === confirm.requireText;

  // Inline "new database" input, opened from the server-row context menu.
  const queryClient = useQueryClient();
  const [creating, setCreating] = useState(false);
  const [createDraft, setCreateDraft] = useState("");

  async function commitCreate() {
    const name = createDraft.trim();
    setCreating(false);
    setCreateDraft("");
    if (!name) return;
    try {
      await sessionCreateDatabase(sessionId, name);
      await queryClient.invalidateQueries({
        queryKey: qk.databases(sessionId),
      });
    } catch (e) {
      toastError("Could not create database", asIpcError(e).message);
    }
  }

  function openServerMenu(e: React.MouseEvent) {
    openMenu(e, [
      {
        label: "New database…",
        disabled: session.readOnly,
        onSelect: () => {
          setCreateDraft("");
          setCreating(true);
          setOpen(true);
        },
      },
    ]);
  }

  return (
    <FilterContext.Provider value={filter}>
      <ConfirmContext.Provider value={requestConfirm}>
        <MenuContext.Provider value={openMenu}>
          <div className={styles.tree} role="tree">
            <div className={styles.serverRow} onContextMenu={openServerMenu}>
              <span
                className={`${styles.caret} ${open ? styles.caretOpen : ""}`}
                onClick={() => setOpen((o) => !o)}
                aria-hidden
              >
                <CaretIcon />
              </span>
              <span className={styles.icon} aria-hidden>
                <ConnectionIcon />
              </span>
              <span
                className={styles.serverName}
                title={session.connectionName}
              >
                {session.connectionName}
              </span>
              <button
                type="button"
                className="ghost"
                title="Disconnect"
                aria-label={`Disconnect ${session.connectionName}`}
                onClick={onDisconnect}
              >
                <DisconnectIcon />
              </button>
            </div>
            {open && (
              <>
                {creating && (
                  <div
                    className={styles.row}
                    style={{ paddingLeft: 1 * 14 + 6 }}
                  >
                    <span className={styles.caretSpacer} aria-hidden />
                    <span className={styles.icon} aria-hidden>
                      <DatabaseIcon />
                    </span>
                    <input
                      className={styles.renameInput}
                      autoFocus
                      value={createDraft}
                      placeholder="database name"
                      aria-label="New database name"
                      onChange={(e) => setCreateDraft(e.target.value)}
                      onClick={(e) => e.stopPropagation()}
                      onKeyDown={(e) => {
                        e.stopPropagation();
                        if (e.key === "Enter") {
                          e.preventDefault();
                          void commitCreate();
                        } else if (e.key === "Escape") {
                          e.preventDefault();
                          setCreating(false);
                          setCreateDraft("");
                        }
                      }}
                      onBlur={() => void commitCreate()}
                    />
                  </div>
                )}
                {isLoading && <Loading depth={1} />}
                {error && <ErrorRow depth={1} />}
                {(data ?? []).map((db) => (
                  <DatabaseNode
                    key={db.name}
                    session={session}
                    database={db}
                    depth={1}
                  />
                ))}
              </>
            )}
          </div>
          <ContextMenu
            open={menuOpen}
            x={menu?.x ?? 0}
            y={menu?.y ?? 0}
            items={menu?.items ?? []}
            onClose={() => setMenuOpen(false)}
          />
          <Modal
            open={confirmOpen}
            title={confirm?.title ?? ""}
            tone="danger"
            onClose={() => setConfirmOpen(false)}
            footer={
              <>
                <button type="button" onClick={() => setConfirmOpen(false)}>
                  Cancel
                </button>
                <button
                  type="button"
                  className="danger"
                  disabled={!confirmReady}
                  onClick={() => {
                    if (!confirmReady) return;
                    setConfirmOpen(false);
                    confirm?.onConfirm();
                  }}
                >
                  {confirm?.confirmLabel ?? "Confirm"}
                </button>
              </>
            }
          >
            <p>{confirm?.message}</p>
            {confirm?.requireText != null && (
              <div className={styles.confirmField}>
                <label htmlFor="confirm-type-to-drop">
                  Type <strong>{confirm.requireText}</strong> to confirm
                </label>
                <input
                  id="confirm-type-to-drop"
                  autoFocus
                  autoComplete="off"
                  spellCheck={false}
                  value={confirmInput}
                  aria-label={`Type ${confirm.requireText} to confirm`}
                  onChange={(e) => setConfirmInput(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && confirmReady) {
                      e.preventDefault();
                      setConfirmOpen(false);
                      confirm?.onConfirm();
                    }
                  }}
                />
              </div>
            )}
          </Modal>
        </MenuContext.Provider>
      </ConfirmContext.Provider>
    </FilterContext.Provider>
  );
}

function Loading({ depth }: { depth: number }) {
  return (
    <div className={styles.meta} style={{ paddingLeft: depth * 14 + 22 }}>
      <span className="spinner" aria-hidden />
      Loading…
    </div>
  );
}

function ErrorRow({ depth }: { depth: number }) {
  return (
    <div
      className={`${styles.meta} ${styles.error}`}
      style={{ paddingLeft: depth * 14 + 22 }}
    >
      Failed to load
    </div>
  );
}

function EmptyRow({ depth, label }: { depth: number; label: string }) {
  return (
    <div className={styles.meta} style={{ paddingLeft: depth * 14 + 22 }}>
      {label}
    </div>
  );
}
