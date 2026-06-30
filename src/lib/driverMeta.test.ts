import { describe, expect, it } from "vitest";

import type { DriverId } from "../ipc/types";
import { DRIVERS, driverDefaultPort, driverLabel } from "./driverMeta";

describe("driverLabel", () => {
  it("maps each driver to its display name", () => {
    expect(driverLabel("mssql")).toBe("SQL Server");
    expect(driverLabel("postgres")).toBe("PostgreSQL");
    expect(driverLabel("mysql")).toBe("MySQL");
    expect(driverLabel("sqlite")).toBe("SQLite");
  });
});

describe("driverDefaultPort", () => {
  it("maps each network driver to its conventional port", () => {
    expect(driverDefaultPort("mssql")).toBe(1433);
    expect(driverDefaultPort("postgres")).toBe(5432);
    expect(driverDefaultPort("mysql")).toBe(3306);
  });

  it("returns null for fileless SQLite", () => {
    expect(driverDefaultPort("sqlite")).toBeNull();
  });
});

describe("DRIVERS", () => {
  it("lists every driver exactly once", () => {
    const expected: DriverId[] = ["mssql", "postgres", "mysql", "sqlite"];
    expect([...DRIVERS].sort()).toEqual([...expected].sort());
  });
});
