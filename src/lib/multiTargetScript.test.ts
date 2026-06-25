import { describe, expect, it } from "vitest";

import { buildScriptText } from "./multiTargetScript";

describe("buildScriptText", () => {
  it("emits a USE / RAISERROR / query / PRINT block per database", () => {
    const out = buildScriptText(["e_alpha", "e_beta"], "SELECT 1");
    expect(out).toContain("USE [e_alpha]");
    expect(out).toContain("USE [e_beta]");
    expect(out).toContain(
      "RAISERROR(N'Running script in database: [e_alpha]', 10, 1) WITH NOWAIT",
    );
    expect(out).toContain("PRINT 'Query executed for e_beta'");
    // One block per database (two USE statements, two PRINTs).
    expect(out.match(/USE \[/g)).toHaveLength(2);
    expect(out.match(/PRINT 'Query executed/g)).toHaveLength(2);
  });

  it("trims the query body and places it between RAISERROR and PRINT", () => {
    const out = buildScriptText(["db1"], "\n  UPDATE t SET x = 1  \n");
    const lines = out.split("\n");
    const raiseIdx = lines.findIndex((l) => l.startsWith("RAISERROR"));
    const bodyIdx = lines.findIndex((l) => l === "UPDATE t SET x = 1");
    const printIdx = lines.findIndex((l) => l.startsWith("PRINT"));
    expect(raiseIdx).toBeGreaterThanOrEqual(0);
    expect(bodyIdx).toBe(raiseIdx + 1);
    expect(printIdx).toBe(bodyIdx + 1);
  });

  it("returns an empty string for no databases", () => {
    expect(buildScriptText([], "SELECT 1")).toBe("");
  });
});
