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

import type {
  ColumnInfo,
  DatabaseInfo,
  SchemaInfo,
  TableInfo,
} from "../ipc/types";
import {
  useColumns,
  useDatabases,
  useSchemas,
  useTables,
} from "../lib/queries";
import { useEditorStore } from "../state/editorStore";
import { useImportStore } from "../state/importStore";
import type { LiveSession } from "../state/sessionStore";
import { ContextMenu, type ContextMenuItem } from "./ContextMenu";
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
}

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
}) {
  const activate = onActivate ?? onToggle;
  return (
    <div
      className={styles.row}
      style={{ paddingLeft: depth * 14 + 6 }}
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
      {(data ?? []).map((col: ColumnInfo) => (
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
  const requestImport = useImportStore((s) => s.requestImport);

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
  const requestImport = useImportStore((s) => s.requestImport);
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
  const sessionId = session.info.sessionId;
  const hasSchemas = session.info.capabilities.schemas;
  return (
    <>
      <Row
        depth={depth}
        expandable
        expanded={open}
        onToggle={() => setOpen((o) => !o)}
        icon={
          database.is_system ? (
            <DatabaseIcon style={{ color: "var(--text-faint)" }} />
          ) : (
            <DatabaseIcon />
          )
        }
        label={database.name}
        title={database.is_system ? `${database.name} (system)` : database.name}
      />
      {open &&
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

export function SchemaTree({ session, onDisconnect }: SchemaTreeProps) {
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

  return (
    <MenuContext.Provider value={openMenu}>
      <div className={styles.tree} role="tree">
        <div className={styles.serverRow}>
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
          <span className={styles.serverName} title={session.connectionName}>
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
    </MenuContext.Provider>
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
