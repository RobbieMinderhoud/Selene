/**
 * Tests for the schema-aware completion helpers.
 *
 * The pure pieces (`q`, `columnCompletion`, `buildNamespace`,
 * `referencedTables`) are asserted directly. The async source is exercised
 * through a real CodeMirror `CompletionContext` (jsdom needs no DOM for the SQL
 * parser) with `columnsList` mocked, to prove the key behaviour: tables are
 * offered with no column fetch, and a table's columns are fetched lazily —
 * once, only for the referenced table — the moment it's referenced.
 */

import { beforeEach, describe, expect, it, vi } from "vitest";

// Mock only the column fetch; the source uses the real queryClient cache.
vi.mock("../ipc/commands", () => ({
  columnsList: vi.fn(),
}));

import { EditorState } from "@codemirror/state";
import { sql, MSSQL } from "@codemirror/lang-sql";
import { CompletionContext } from "@codemirror/autocomplete";
import type { CompletionResult } from "@codemirror/autocomplete";

import { columnsList } from "../ipc/commands";
import type { ColumnInfo } from "../ipc/types";
import type { TableRef } from "./queries";
import { queryClient } from "./queryClient";
import {
  buildNamespace,
  columnCompletion,
  makeSchemaCompletionSource,
  q,
  referencedTables,
} from "./sqlCompletion";

const col = (name: string, extra: Partial<ColumnInfo> = {}): ColumnInfo => ({
  name,
  ordinal: 1,
  data_type: "int",
  nullable: true,
  is_primary_key: false,
  max_length: null,
  ...extra,
});

const TABLES: TableRef[] = [
  { schema: "dbo", name: "accountingoffer", kind: "table" },
  { schema: "dbo", name: "crmdossier", kind: "table" },
  { schema: "sales", name: "Order Details", kind: "view" },
];

function ctxFor(doc: string, pos = doc.length, explicit = true) {
  const state = EditorState.create({
    doc,
    extensions: [sql({ dialect: MSSQL })],
  });
  return new CompletionContext(state, pos, explicit);
}

const labels = (r: CompletionResult | null) =>
  (r?.options ?? []).map((o) => o.label);

describe("q (bracket-when-needed quoting)", () => {
  it("leaves simple identifiers bare", () => {
    expect(q("accountingoffer")).toBe("accountingoffer");
    expect(q("crm_dossier1")).toBe("crm_dossier1");
  });

  it("brackets names with spaces or punctuation", () => {
    expect(q("Order Details")).toBe("[Order Details]");
    expect(q("weird-name")).toBe("[weird-name]");
  });

  it("brackets names starting with a digit", () => {
    expect(q("1table")).toBe("[1table]");
  });

  it("brackets reserved keywords (case-insensitively)", () => {
    expect(q("user")).toBe("[user]");
    expect(q("Order")).toBe("[Order]");
    expect(q("SELECT")).toBe("[SELECT]");
  });

  it("doubles a literal closing bracket", () => {
    expect(q("a]b")).toBe("[a]]b]");
  });
});

describe("columnCompletion", () => {
  it("marks primary keys as constants that sort first", () => {
    const c = columnCompletion(
      col("id", { is_primary_key: true, nullable: false }),
    );
    expect(c.type).toBe("constant");
    expect(c.boost).toBe(1);
    expect(c.detail).toContain("int");
    expect(c.detail).toContain("not null");
    expect(c.apply).toBe("id");
  });

  it("marks ordinary columns as properties with the type as detail", () => {
    const c = columnCompletion(
      col("name", { data_type: "nvarchar", max_length: 50 }),
    );
    expect(c.type).toBe("property");
    expect(c.boost).toBe(0);
    expect(c.detail).toBe("nvarchar(50)");
  });

  it("quotes column names that need it", () => {
    expect(columnCompletion(col("Total Amount")).apply).toBe("[Total Amount]");
  });
});

// Structural view of the namespace, for asserting on its shape in tests.
interface LeafCompletion {
  label: string;
  apply?: string;
}
interface NsTable {
  self: { label: string; type: string; detail?: string; apply?: string };
  children: LeafCompletion[];
}
interface NsSchema {
  self: { label: string; type: string; detail?: string; apply?: string };
  children: Record<string, NsTable>;
}
type Ns = Record<string, NsSchema>;

describe("buildNamespace", () => {
  it("nests schema → table with self labels equal to their keys", () => {
    const ns = buildNamespace(TABLES, new Map()) as unknown as Ns;
    expect(ns.dbo.self.label).toBe("dbo");
    expect(ns.dbo.self.type).toBe("namespace");
    expect(ns.dbo.children.accountingoffer.self.label).toBe("accountingoffer");
    expect(ns.dbo.children.accountingoffer.self.detail).toBe("table");
    // No columns loaded yet → empty children.
    expect(ns.dbo.children.accountingoffer.children).toEqual([]);
    expect(ns.sales.children["Order Details"].self.apply).toBe(
      "[Order Details]",
    );
  });

  it("fills columns for tables present in the columns map", () => {
    const cols = new Map([["dbo.accountingoffer", [col("id"), col("amount")]]]);
    const ns = buildNamespace(TABLES, cols) as unknown as Ns;
    expect(
      ns.dbo.children.accountingoffer.children.map((c) => c.label),
    ).toEqual(["id", "amount"]);
    expect(ns.dbo.children.crmdossier.children).toEqual([]);
  });
});

describe("referencedTables", () => {
  it("returns nothing for an empty FROM", () => {
    expect(referencedTables("SELECT * FROM ", TABLES)).toEqual([]);
  });

  it("matches a bare table name", () => {
    const refs = referencedTables("SELECT * FROM accountingoffer", TABLES);
    expect(refs.map((t) => t.name)).toEqual(["accountingoffer"]);
  });

  it("matches the real table behind an alias", () => {
    const refs = referencedTables(
      "SELECT ao.x FROM accountingoffer ao",
      TABLES,
    );
    expect(refs.map((t) => t.name)).toEqual(["accountingoffer"]);
  });

  it("matches bracketed identifiers", () => {
    const refs = referencedTables("SELECT * FROM [Order Details]", TABLES);
    expect(refs.map((t) => t.name)).toEqual(["Order Details"]);
  });
});

describe("makeSchemaCompletionSource", () => {
  beforeEach(() => {
    queryClient.clear();
    vi.mocked(columnsList).mockReset();
    vi.mocked(columnsList).mockImplementation((_s, _d, _schema, table) =>
      Promise.resolve(
        table === "accountingoffer"
          ? [col("id", { is_primary_key: true }), col("amount")]
          : [],
      ),
    );
  });

  it("offers tables after FROM without fetching any columns", async () => {
    const source = makeSchemaCompletionSource({
      sessionId: "s1",
      database: "db1",
      tables: TABLES,
    });
    const res = (await source(ctxFor("SELECT * FROM "))) as CompletionResult;
    const names = labels(res);
    expect(names).toContain("accountingoffer");
    expect(names).toContain("crmdossier");
    expect(columnsList).not.toHaveBeenCalled();
  });

  it("lazily fetches columns for the referenced table only", async () => {
    const source = makeSchemaCompletionSource({
      sessionId: "s1",
      database: "db1",
      tables: TABLES,
    });
    const doc = "SELECT * FROM accountingoffer WHERE accountingoffer.";
    const res = (await source(ctxFor(doc))) as CompletionResult;

    expect(columnsList).toHaveBeenCalledTimes(1);
    expect(columnsList).toHaveBeenCalledWith(
      "s1",
      "db1",
      "dbo",
      "accountingoffer",
    );
    expect(labels(res)).toEqual(expect.arrayContaining(["id", "amount"]));
  });
});
