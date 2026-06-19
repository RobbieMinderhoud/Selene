import { describe, expect, it } from "vitest";

import { matchConnectionForFile } from "./connectionMatch";
import type { NamedConnection } from "./connectionMatch";

const conn = (id: string, name: string): NamedConnection => ({ id, name });

describe("matchConnectionForFile", () => {
  it("matches the connection whose name is a token in the file name", () => {
    const connections = [conn("c1", "pr02db02b"), conn("c2", "acc01")];
    expect(matchConnectionForFile("pr02db02b_shared_01.sql", connections)).toBe(
      "c1",
    );
  });

  it("matches case-insensitively", () => {
    const connections = [conn("c1", "pr02db02b")];
    expect(matchConnectionForFile("PR02DB02B_SHARED.SQL", connections)).toBe(
      "c1",
    );
  });

  it("matches a name at the start or end of the file name", () => {
    const connections = [conn("c1", "pr02db02b")];
    expect(matchConnectionForFile("pr02db02b.sql", connections)).toBe("c1");
    expect(matchConnectionForFile("query_pr02db02b.sql", connections)).toBe(
      "c1",
    );
  });

  it("does not match a name embedded inside a longer alphanumeric run", () => {
    const connections = [conn("c1", "pr02db02b")];
    // No non-alphanumeric boundary around the name → not a real token.
    expect(matchConnectionForFile("xpr02db02bx.sql", connections)).toBeNull();
  });

  it("does not match a shorter, different server that is a prefix", () => {
    // `pr02db02` is followed by `b` (alphanumeric) in the file name, so it is
    // a substring but NOT a token — a different, shorter server must not match.
    const connections = [conn("c1", "pr02db02")];
    expect(
      matchConnectionForFile("pr02db02b_shared.sql", connections),
    ).toBeNull();
  });

  it("prefers the longest (most specific) matching name", () => {
    const connections = [conn("short", "pr02"), conn("long", "pr02db02b")];
    // Both occur as tokens here; the longer, more specific one wins.
    expect(matchConnectionForFile("pr02_pr02db02b.sql", connections)).toBe(
      "long",
    );
  });

  it("returns null when no connection name appears", () => {
    const connections = [conn("c1", "pr02db02b"), conn("c2", "acc01")];
    expect(matchConnectionForFile("report_final.sql", connections)).toBeNull();
  });

  it("returns null for an empty connection list", () => {
    expect(matchConnectionForFile("pr02db02b.sql", [])).toBeNull();
  });

  it("ignores connections with a blank name", () => {
    const connections = [conn("blank", "   "), conn("c1", "pr02db02b")];
    expect(matchConnectionForFile("pr02db02b.sql", connections)).toBe("c1");
  });
});
