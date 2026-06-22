import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { useTypeToFilter } from "./useTypeToFilter";

/** Build a minimal React.KeyboardEvent-like object for the handler. */
function keyEvent(over: Partial<Record<string, unknown>> = {}) {
  return {
    key: "a",
    ctrlKey: false,
    metaKey: false,
    altKey: false,
    preventDefault: vi.fn(),
    target: { tagName: "DIV", isContentEditable: false },
    ...over,
  } as unknown as React.KeyboardEvent;
}

describe("useTypeToFilter", () => {
  it("appends printable characters", () => {
    const { result } = renderHook(() => useTypeToFilter());
    act(() => result.current.onKeyDown(keyEvent({ key: "c" })));
    act(() => result.current.onKeyDown(keyEvent({ key: "u" })));
    expect(result.current.query).toBe("cu");
  });

  it("Backspace pops the last character", () => {
    const { result } = renderHook(() => useTypeToFilter());
    act(() => result.current.onKeyDown(keyEvent({ key: "a" })));
    act(() => result.current.onKeyDown(keyEvent({ key: "b" })));
    act(() => result.current.onKeyDown(keyEvent({ key: "Backspace" })));
    expect(result.current.query).toBe("a");
  });

  it("Escape clears the query", () => {
    const { result } = renderHook(() => useTypeToFilter());
    act(() => result.current.onKeyDown(keyEvent({ key: "x" })));
    act(() => result.current.onKeyDown(keyEvent({ key: "Escape" })));
    expect(result.current.query).toBe("");
  });

  it("ignores keystrokes from editable fields (inline rename)", () => {
    const { result } = renderHook(() => useTypeToFilter());
    act(() =>
      result.current.onKeyDown(
        keyEvent({ key: "z", target: { tagName: "INPUT" } }),
      ),
    );
    expect(result.current.query).toBe("");
  });

  it("ignores modifier combinations so shortcuts pass through", () => {
    const { result } = renderHook(() => useTypeToFilter());
    act(() => result.current.onKeyDown(keyEvent({ key: "a", metaKey: true })));
    act(() => result.current.onKeyDown(keyEvent({ key: "Enter" })));
    act(() => result.current.onKeyDown(keyEvent({ key: "ArrowDown" })));
    expect(result.current.query).toBe("");
  });
});
