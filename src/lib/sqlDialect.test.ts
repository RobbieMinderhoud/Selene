import { describe, expect, it } from "vitest";
import {
  MSSQL,
  MySQL,
  PostgreSQL,
  SQLite,
  StandardSQL,
} from "@codemirror/lang-sql";

import { dialectFor } from "./sqlDialect";

describe("dialectFor", () => {
  it("maps each driver to its lang-sql dialect", () => {
    expect(dialectFor("mssql")).toBe(MSSQL);
    expect(dialectFor("postgres")).toBe(PostgreSQL);
    expect(dialectFor("mysql")).toBe(MySQL);
    expect(dialectFor("sqlite")).toBe(SQLite);
  });

  it("falls back to StandardSQL when no driver is known", () => {
    expect(dialectFor(undefined)).toBe(StandardSQL);
  });
});
