import { describe, expect, it } from "vitest";

import { asIpcError } from "./types";

describe("asIpcError", () => {
  it("passes through a well-formed IpcError", () => {
    expect(asIpcError({ message: "boom", kind: "query" })).toEqual({
      message: "boom",
      kind: "query",
    });
  });

  it("defaults kind to 'unknown' when missing", () => {
    expect(asIpcError({ message: "boom" })).toEqual({
      message: "boom",
      kind: "unknown",
    });
  });

  it("wraps a thrown string", () => {
    expect(asIpcError("oops")).toEqual({ message: "oops", kind: "unknown" });
  });

  it("falls back for an unrecognized value", () => {
    expect(asIpcError(null)).toMatchObject({ kind: "unknown" });
    expect(asIpcError(42)).toMatchObject({ kind: "unknown" });
  });
});
