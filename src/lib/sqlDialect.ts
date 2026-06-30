/**
 * Maps a connection's {@link DriverId} to the matching `@codemirror/lang-sql`
 * dialect, so the editor highlights and completes keywords for the backend the
 * tab is actually talking to. Falls back to `StandardSQL` when there's no
 * connected session yet (the tab has no driver).
 */

import {
  MSSQL,
  MySQL,
  PostgreSQL,
  SQLite,
  StandardSQL,
} from "@codemirror/lang-sql";
import type { SQLDialect } from "@codemirror/lang-sql";

import type { DriverId } from "../ipc/types";

/** The lang-sql dialect for a driver; `StandardSQL` when no driver is known. */
export function dialectFor(d: DriverId | undefined): SQLDialect {
  switch (d) {
    case "mssql":
      return MSSQL;
    case "postgres":
      return PostgreSQL;
    case "mysql":
      return MySQL;
    case "sqlite":
      return SQLite;
    default:
      return StandardSQL;
  }
}
