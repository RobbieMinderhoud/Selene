/**
 * Small, pure helpers describing each supported backend driver: a human-facing
 * label and its conventional default TCP port. Mirrors the Rust
 * `DriverId::default_port` (SQLite is fileless, so it has no port).
 */

import type { DriverId } from "../ipc/types";

/** Display name for a driver, e.g. for the connection dialog and tab toolbar. */
export function driverLabel(d: DriverId): string {
  switch (d) {
    case "mssql":
      return "SQL Server";
    case "postgres":
      return "PostgreSQL";
    case "mysql":
      return "MySQL";
    case "sqlite":
      return "SQLite";
  }
}

/** The conventional default port for a driver, or `null` when it uses none. */
export function driverDefaultPort(d: DriverId): number | null {
  switch (d) {
    case "mssql":
      return 1433;
    case "postgres":
      return 5432;
    case "mysql":
      return 3306;
    case "sqlite":
      return null;
  }
}

/** All drivers, in the order they appear in the connection dialog's select. */
export const DRIVERS: DriverId[] = ["mssql", "postgres", "mysql", "sqlite"];
