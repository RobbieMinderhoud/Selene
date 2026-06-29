import { describe, expect, it } from "vitest";

import type { CellValue } from "../ipc/types";
import { buildClipboard, copyCellText } from "./gridClipboard";

const HEADERS = ["id", "name"];
// A 2×2 rectangle, including a value that needs escaping in every format.
const MATRIX = [
  ["1", "a,b"],
  ["2", 'x"y'],
];

describe("copyCellText", () => {
  it("returns empty string for NULL and missing cells", () => {
    expect(copyCellText({ t: "Null" } as CellValue)).toBe("");
    expect(copyCellText(undefined)).toBe("");
  });

  it("renders a value via formatCell", () => {
    expect(copyCellText({ t: "I64", v: 42 })).toBe("42");
  });
});

describe("buildClipboard", () => {
  it("tab format joins cells with TAB and rows with CRLF (Excel-friendly)", () => {
    const { text, html } = buildClipboard("tab", HEADERS, MATRIX, false);
    expect(html).toBeUndefined();
    expect(text).toBe('1\ta,b\r\n2\tx"y');
  });

  it("includes the header row only when asked", () => {
    expect(buildClipboard("tab", HEADERS, MATRIX, true).text).toBe(
      'id\tname\r\n1\ta,b\r\n2\tx"y',
    );
  });

  it("comma format quotes fields with commas/quotes and doubles quotes", () => {
    const { text } = buildClipboard("comma", HEADERS, MATRIX, false);
    expect(text).toBe('1,"a,b"\r\n2,"x""y"');
  });

  it("markdown always emits a header + separator row", () => {
    const { text } = buildClipboard("markdown", HEADERS, MATRIX, false);
    expect(text).toBe(
      ["| id | name |", "| --- | --- |", "| 1 | a,b |", '| 2 | x"y |'].join(
        "\n",
      ),
    );
  });

  it("markdown escapes pipes and flattens newlines", () => {
    const { text } = buildClipboard("markdown", ["c"], [["a|b\nc"]], false);
    expect(text).toContain("| a\\|b<br>c |");
  });

  it("html builds a table and escapes markup, with a TSV plain fallback", () => {
    const { text, html } = buildClipboard("html", ["c"], [["<b>&"]], true);
    expect(html).toBe(
      "<table><thead><tr><th>c</th></tr></thead><tbody><tr><td>&lt;b&gt;&amp;</td></tr></tbody></table>",
    );
    // Plain fallback is the TSV form (with the header, since includeHeaders).
    expect(text).toBe("c\r\n<b>&");
  });

  it("html omits thead when headers are off", () => {
    const { html } = buildClipboard("html", ["c"], [["v"]], false);
    expect(html).toBe("<table><tbody><tr><td>v</td></tr></tbody></table>");
  });
});
