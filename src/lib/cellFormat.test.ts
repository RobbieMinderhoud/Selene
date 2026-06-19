import { describe, expect, it } from "vitest";

import type { CellValue } from "../ipc/types";
import { formatCell } from "./cellFormat";

describe("formatCell", () => {
  it("renders NULL as a dim left-aligned marker", () => {
    const f = formatCell({ t: "Null" });
    expect(f).toEqual({ text: "NULL", isNull: true, align: "left" });
  });

  it("renders booleans as true/false", () => {
    expect(formatCell({ t: "Bool", v: true }).text).toBe("true");
    expect(formatCell({ t: "Bool", v: false }).text).toBe("false");
  });

  it("right-aligns numeric variants", () => {
    expect(formatCell({ t: "I64", v: 42 })).toMatchObject({
      text: "42",
      align: "right",
    });
    expect(formatCell({ t: "F64", v: 3.5 })).toMatchObject({
      text: "3.5",
      align: "right",
    });
    // Decimal stays a string (exact precision preserved on the wire).
    expect(formatCell({ t: "Decimal", v: "12345.6789" })).toMatchObject({
      text: "12345.6789",
      align: "right",
    });
  });

  it("renders bytes as uppercase 0x hex", () => {
    const f = formatCell({ t: "Bytes", v: [0x00, 0x0f, 0xab, 0xff] });
    expect(f.text).toBe("0x000FABFF");
    expect(f.align).toBe("left");
  });

  it("truncates long byte blobs with a count", () => {
    const bytes = Array.from({ length: 64 }, () => 0xab);
    const f = formatCell({ t: "Bytes", v: bytes });
    expect(f.text.startsWith("0x")).toBe(true);
    expect(f.text).toContain("(64 bytes)");
  });

  it("renders DateTime as its ISO string", () => {
    const f = formatCell({
      t: "DateTime",
      v: { iso: "2024-01-02T03:04:05Z", kind: "date_time" },
    });
    expect(f.text).toBe("2024-01-02T03:04:05Z");
  });

  it("renders Uuid and String verbatim", () => {
    expect(formatCell({ t: "Uuid", v: "abc-123" }).text).toBe("abc-123");
    expect(formatCell({ t: "String", v: "hello" }).text).toBe("hello");
  });

  it("renders Unsupported via its preserved text", () => {
    const f = formatCell({
      t: "Unsupported",
      v: { type_name: "geography", text: "POINT(1 2)" },
    });
    expect(f.text).toBe("POINT(1 2)");
  });

  it("handles every documented variant without throwing", () => {
    const samples: CellValue[] = [
      { t: "Null" },
      { t: "Bool", v: true },
      { t: "I64", v: 1 },
      { t: "F64", v: 1.1 },
      { t: "Decimal", v: "1.1" },
      { t: "String", v: "x" },
      { t: "Bytes", v: [1] },
      { t: "DateTime", v: { iso: "2024", kind: "date" } },
      { t: "Uuid", v: "u" },
      { t: "Unsupported", v: { type_name: "t", text: "x" } },
    ];
    for (const s of samples) {
      expect(() => formatCell(s)).not.toThrow();
    }
  });
});
