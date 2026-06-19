/**
 * TanStack Query hooks for cacheable reads (the lazy schema tree + connections).
 *
 * Introspection is lazy per level: each hook is `enabled` only when its parent
 * node is expanded (the caller passes `enabled`), so a server with thousands of
 * objects is never fetched eagerly. Results are cached and keyed by the ids
 * involved, so re-expanding a node is instant.
 */

import { useRef } from "react";
import { useQueries, useQuery } from "@tanstack/react-query";

import {
  columnsList,
  connectionsList,
  databasesList,
  dirList,
  schemasList,
  tablesList,
} from "../ipc/commands";
import type {
  ColumnInfo,
  ConnectionSpec,
  DatabaseInfo,
  FsEntry,
  SchemaInfo,
  TableInfo,
  TableKind,
} from "../ipc/types";

/** Stable query-key factory so invalidation/refetch is consistent. */
export const qk = {
  connections: () => ["connections"] as const,
  databases: (sessionId: string) => ["databases", sessionId] as const,
  schemas: (sessionId: string, database: string) =>
    ["schemas", sessionId, database] as const,
  tables: (sessionId: string, database: string, schema: string) =>
    ["tables", sessionId, database, schema] as const,
  columns: (
    sessionId: string,
    database: string,
    schema: string,
    table: string,
  ) => ["columns", sessionId, database, schema, table] as const,
  /** Directory listing for the workspace file tree, keyed by folder path. */
  dir: (path: string) => ["dir", path] as const,
};

export function useConnections() {
  return useQuery<ConnectionSpec[]>({
    queryKey: qk.connections(),
    queryFn: connectionsList,
  });
}

export function useDatabases(sessionId: string, enabled: boolean) {
  return useQuery<DatabaseInfo[]>({
    queryKey: qk.databases(sessionId),
    queryFn: () => databasesList(sessionId),
    enabled,
  });
}

export function useSchemas(
  sessionId: string,
  database: string,
  enabled: boolean,
) {
  return useQuery<SchemaInfo[]>({
    queryKey: qk.schemas(sessionId, database),
    queryFn: () => schemasList(sessionId, database),
    enabled,
  });
}

export function useTables(
  sessionId: string,
  database: string,
  schema: string,
  enabled: boolean,
) {
  return useQuery<TableInfo[]>({
    queryKey: qk.tables(sessionId, database, schema),
    queryFn: () => tablesList(sessionId, database, schema),
    enabled,
  });
}

export function useColumns(
  sessionId: string,
  database: string,
  schema: string,
  table: string,
  enabled: boolean,
) {
  return useQuery<ColumnInfo[]>({
    queryKey: qk.columns(sessionId, database, schema, table),
    queryFn: () => columnsList(sessionId, database, schema, table),
    enabled,
  });
}

/** Lazy directory listing for the workspace file tree (per expanded folder). */
export function useDir(path: string, enabled: boolean) {
  return useQuery<FsEntry[]>({
    queryKey: qk.dir(path),
    queryFn: () => dirList(path),
    enabled,
  });
}

/** A schema-qualified table/view, flattened across all schemas of a database. */
export interface TableRef {
  schema: string;
  name: string;
  kind: TableKind;
}

/**
 * All tables/views of `(sessionId, database)`, flattened across its schemas, for
 * schema-aware editor autocomplete. Schemas + tables are loaded eagerly (cheap:
 * one query per schema, cached) so `FROM`-completion is instant; columns stay
 * lazy and are fetched on demand by the completion source itself.
 *
 * Returns a **referentially stable** array: a new reference is produced only when
 * the set of `(schema, table, kind)` actually changes. EditorPane re-renders on
 * every keystroke (the tab object is replaced on edit), so this stability is what
 * keeps the downstream completion-source memo — and thus the CodeMirror
 * reconfigure — from rebuilding on each character.
 */
export function useSchemaTables(
  sessionId: string | null,
  database: string | null,
  enabled: boolean,
): TableRef[] {
  const on = enabled && !!sessionId && !!database;
  const sid = sessionId ?? "";
  const db = database ?? "";

  const { data: schemas } = useQuery<SchemaInfo[]>({
    queryKey: qk.schemas(sid, db),
    queryFn: () => schemasList(sid, db),
    enabled: on,
  });

  const tableQueries = useQueries({
    queries: (schemas ?? []).map((s) => ({
      queryKey: qk.tables(sid, db, s.name),
      queryFn: () => tablesList(sid, db, s.name),
      enabled: on,
    })),
  });

  const flat: TableRef[] = [];
  for (const tq of tableQueries) {
    for (const t of tq.data ?? []) {
      flat.push({ schema: t.schema, name: t.name, kind: t.kind });
    }
  }
  // Derive a stable reference keyed on content: TanStack returns a fresh result
  // array each render, but the underlying `data` only changes on (re)fetch.
  const sig = flat.map((t) => `${t.schema}.${t.name}.${t.kind}`).join("|");
  const ref = useRef<{ sig: string; value: TableRef[] }>({
    sig: "",
    value: [],
  });
  if (ref.current.sig !== sig) ref.current = { sig, value: flat };
  return ref.current.value;
}
