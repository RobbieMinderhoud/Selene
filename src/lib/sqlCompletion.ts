/**
 * Schema-aware SQL autocomplete: turns introspected tables/columns into a
 * CodeMirror completion source.
 *
 * The heavy lifting (knowing that we're after `FROM`/`JOIN`, resolving
 * `alias.col`, dotted `schema.table`, filtering as you type) is done by
 * `@codemirror/lang-sql`'s own `schemaCompletionSource`. We only:
 *   1. build the `SQLNamespace` it consumes from our `TableRef[]` + columns, and
 *   2. wrap it in an async source that lazily fetches a table's columns the
 *      first time that table is referenced in the query — so opening a query
 *      never eagerly pulls columns for every table on the server.
 *
 * Tables are supplied eagerly by the caller (`useSchemaTables`); columns are
 * fetched on demand here, through the shared TanStack cache, so a column list
 * is read at most once per `staleTime` per `(session, database, schema, table)`.
 */

import { MSSQL, schemaCompletionSource } from "@codemirror/lang-sql";
import type { SQLNamespace } from "@codemirror/lang-sql";
import type {
  Completion,
  CompletionContext,
  CompletionResult,
  CompletionSource,
} from "@codemirror/autocomplete";

import { columnsList } from "../ipc/commands";
import type { ColumnInfo } from "../ipc/types";
import { qk, type TableRef } from "./queries";
import { queryClient } from "./queryClient";

/**
 * T-SQL reserved keywords that are also plausible identifier names. Used by
 * `q()` so a table/column literally named e.g. `User` or `Order` is still
 * bracketed (those are reserved and would be a syntax error unquoted).
 * Not exhaustive of the full reserved list — just the ones likely to collide
 * with real object names.
 */
const RESERVED = new Set([
  "add",
  "all",
  "alter",
  "and",
  "any",
  "as",
  "asc",
  "authorization",
  "backup",
  "begin",
  "between",
  "break",
  "browse",
  "bulk",
  "by",
  "cascade",
  "case",
  "check",
  "checkpoint",
  "close",
  "clustered",
  "coalesce",
  "collate",
  "column",
  "commit",
  "compute",
  "constraint",
  "contains",
  "continue",
  "convert",
  "create",
  "cross",
  "current",
  "cursor",
  "database",
  "dbcc",
  "deallocate",
  "declare",
  "default",
  "delete",
  "deny",
  "desc",
  "disk",
  "distinct",
  "distributed",
  "double",
  "drop",
  "else",
  "end",
  "errlvl",
  "escape",
  "except",
  "exec",
  "execute",
  "exists",
  "exit",
  "external",
  "fetch",
  "file",
  "fillfactor",
  "for",
  "foreign",
  "freetext",
  "from",
  "full",
  "function",
  "goto",
  "grant",
  "group",
  "having",
  "holdlock",
  "identity",
  "if",
  "in",
  "index",
  "inner",
  "insert",
  "intersect",
  "into",
  "is",
  "join",
  "key",
  "kill",
  "left",
  "like",
  "lineno",
  "merge",
  "national",
  "nocheck",
  "nonclustered",
  "not",
  "null",
  "nullif",
  "of",
  "off",
  "on",
  "open",
  "option",
  "or",
  "order",
  "outer",
  "over",
  "percent",
  "pivot",
  "plan",
  "precision",
  "primary",
  "print",
  "proc",
  "procedure",
  "public",
  "raiserror",
  "read",
  "readtext",
  "reconfigure",
  "references",
  "replication",
  "restore",
  "restrict",
  "return",
  "revert",
  "revoke",
  "right",
  "rollback",
  "rowcount",
  "rowguidcol",
  "rule",
  "save",
  "schema",
  "select",
  "session_user",
  "set",
  "setuser",
  "shutdown",
  "some",
  "statistics",
  "system_user",
  "table",
  "tablesample",
  "textsize",
  "then",
  "to",
  "top",
  "tran",
  "transaction",
  "trigger",
  "truncate",
  "union",
  "unique",
  "unpivot",
  "update",
  "updatetext",
  "use",
  "user",
  "values",
  "varying",
  "view",
  "waitfor",
  "when",
  "where",
  "while",
  "with",
  "writetext",
]);

/**
 * Bracket-quote an MSSQL identifier only when needed: bare for a simple,
 * non-reserved identifier; `[...]` otherwise (spaces, punctuation, reserved
 * words). Mirrors the Rust `quote_ident` (a literal `]` is doubled).
 */
export function q(name: string): string {
  const simple = /^[A-Za-z_][A-Za-z0-9_]*$/.test(name);
  if (simple && !RESERVED.has(name.toLowerCase())) return name;
  return "[" + name.replace(/]/g, "]]") + "]";
}

/** A column → a CodeMirror completion (PKs sort first, type shown as detail). */
export function columnCompletion(c: ColumnInfo): Completion {
  const len =
    c.max_length != null && c.max_length > 0 ? `(${c.max_length})` : "";
  return {
    label: c.name,
    type: c.is_primary_key ? "constant" : "property",
    detail: `${c.data_type}${len}${c.nullable ? "" : " not null"}`,
    apply: q(c.name),
    boost: c.is_primary_key ? 1 : 0,
  };
}

function tableKey(t: TableRef): string {
  return `${t.schema}.${t.name}`;
}

/**
 * Build the nested `SQLNamespace` lang-sql consumes: `schema → table → columns`.
 * Columns for a table are included only if present in `columns` (lazily loaded);
 * otherwise the table still completes with an empty column list.
 *
 * Each level carries an explicit `apply` (via `q()`) so identifiers are quoted
 * the Selene way (brackets), not lang-sql's default double-quotes. `self.label`
 * equals the namespace key so lang-sql doesn't also emit a duplicate auto-entry.
 */
export function buildNamespace(
  tables: TableRef[],
  columns: Map<string, ColumnInfo[]>,
): SQLNamespace {
  const root: Record<
    string,
    {
      self: Completion;
      children: Record<string, { self: Completion; children: Completion[] }>;
    }
  > = {};

  for (const t of tables) {
    let schemaNode = root[t.schema];
    if (!schemaNode) {
      schemaNode = {
        self: { label: t.schema, type: "namespace", apply: q(t.schema) },
        children: {},
      };
      root[t.schema] = schemaNode;
    }
    const cols = columns.get(tableKey(t));
    schemaNode.children[t.name] = {
      self: { label: t.name, type: "type", detail: t.kind, apply: q(t.name) },
      children: cols ? cols.map(columnCompletion) : [],
    };
  }

  return root as unknown as SQLNamespace;
}

/**
 * Tables/views referenced anywhere in `docText` (matched by name,
 * case-insensitively, against the known table set). Both bare identifiers and
 * `[bracketed names]` are scanned. This is what gates the lazy column fetch:
 * the real table name appears in the `FROM`/`JOIN` clause even when the user
 * later types through an alias, so scanning the whole statement covers
 * `alias.`, `table.`, and bare-column-in-SELECT cases alike.
 */
export function referencedTables(
  docText: string,
  tables: TableRef[],
): TableRef[] {
  if (tables.length === 0) return [];
  const byName = new Map<string, TableRef[]>();
  for (const t of tables) {
    const k = t.name.toLowerCase();
    const arr = byName.get(k);
    if (arr) arr.push(t);
    else byName.set(k, [t]);
  }

  const found = new Map<string, TableRef>();
  const tokenRe = /\[([^\]]+)\]|([A-Za-z_][A-Za-z0-9_]*)/g;
  let m: RegExpExecArray | null;
  while ((m = tokenRe.exec(docText)) !== null) {
    const raw = (m[1] ?? m[2] ?? "").toLowerCase();
    const matches = byName.get(raw);
    if (matches) {
      for (const t of matches) found.set(tableKey(t), t);
    }
  }
  return [...found.values()];
}

export interface SchemaCompletionOptions {
  sessionId: string;
  database: string;
  /** Eagerly-loaded tables/views for the active database. */
  tables: TableRef[];
  /** Schema whose tables complete without a prefix. Defaults to `dbo`. */
  defaultSchema?: string;
}

/**
 * Build an async completion source for the given session/database/table set.
 *
 * On each request it figures out which known tables the statement references,
 * ensures their columns are loaded (cache hit when already fetched), then
 * delegates to lang-sql's `schemaCompletionSource` with a namespace that now
 * contains those columns. Loaded columns accumulate across requests, and the
 * delegated source is rebuilt only when that loaded set grows.
 */
export function makeSchemaCompletionSource(
  opts: SchemaCompletionOptions,
): CompletionSource {
  const { sessionId, database, tables, defaultSchema = "dbo" } = opts;
  const loaded = new Map<string, ColumnInfo[]>();
  let cache: { sig: string; source: CompletionSource } | null = null;

  return async (ctx: CompletionContext): Promise<CompletionResult | null> => {
    const refs = referencedTables(ctx.state.doc.toString(), tables);
    await Promise.all(
      refs
        .filter((t) => !loaded.has(tableKey(t)))
        .map(async (t) => {
          try {
            const cols = await queryClient.ensureQueryData({
              queryKey: qk.columns(sessionId, database, t.schema, t.name),
              queryFn: () => columnsList(sessionId, database, t.schema, t.name),
            });
            loaded.set(tableKey(t), cols);
          } catch {
            // Cache an empty list so a failed fetch isn't retried every keystroke.
            loaded.set(tableKey(t), []);
          }
        }),
    );

    const sig = [...loaded.keys()].sort().join("|");
    if (!cache || cache.sig !== sig) {
      cache = {
        sig,
        source: schemaCompletionSource({
          dialect: MSSQL,
          schema: buildNamespace(tables, loaded),
          defaultSchema,
        }),
      };
    }
    return cache.source(ctx);
  };
}
