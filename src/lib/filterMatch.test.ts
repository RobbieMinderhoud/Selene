import { describe, expect, it } from "vitest";

import { matches } from "./filterMatch";

describe("matches", () => {
  it("matches everything on an empty or blank query", () => {
    expect(matches("Customers", "")).toBe(true);
    expect(matches("Customers", "   ")).toBe(true);
    expect(matches("", "")).toBe(true);
  });

  it("is a case-insensitive substring test", () => {
    expect(matches("Customers", "cust")).toBe(true);
    expect(matches("Customers", "MER")).toBe(true);
    expect(matches("Customers", "xyz")).toBe(false);
  });

  it("trims the query before matching", () => {
    expect(matches("Customers", "  cust ")).toBe(true);
  });
});
