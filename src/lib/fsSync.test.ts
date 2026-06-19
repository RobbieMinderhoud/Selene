/**
 * The file-sync decision + conflict queue — the correctness core of two-way
 * sync. `reconcile` is pure; the queue drives the conflict modal. (The listener
 * wiring itself is exercised via manual E2E.)
 */

import { beforeEach, describe, expect, it } from "vitest";

import { useEditorStore } from "../state/editorStore";
import { reconcile, useConflictStore } from "./fsSync";

describe("reconcile", () => {
  it("noops when disk matches last-saved (echo of our own write)", () => {
    // Identical content: nothing changed.
    expect(reconcile({ sql: "X", savedSql: "X" }, "X")).toBe("noop");
    // Buffer edited but disk still equals what we last saved -> our own save
    // echo (or unrelated event): suppressed regardless of the dirty buffer.
    expect(reconcile({ sql: "edited", savedSql: "saved" }, "saved")).toBe(
      "noop",
    );
  });

  it("reloads when the buffer is clean and disk differs", () => {
    expect(reconcile({ sql: "A", savedSql: "A" }, "B")).toBe("reload");
  });

  it("conflicts when the buffer is dirty and disk differs", () => {
    expect(reconcile({ sql: "mine", savedSql: "A" }, "theirs")).toBe(
      "conflict",
    );
  });
});

describe("conflict queue", () => {
  beforeEach(() => {
    useConflictStore.setState({ queue: [] });
    useEditorStore.setState({ tabs: [], activeTabId: null, results: {} });
  });

  it("dedupes by path and refreshes onDisk to the latest", () => {
    const s = () => useConflictStore.getState();
    s().enqueue({ tabId: "t1", path: "/a.sql", onDisk: "v1" });
    s().enqueue({ tabId: "t1", path: "/a.sql", onDisk: "v2" });
    expect(s().queue).toHaveLength(1);
    expect(s().queue[0].onDisk).toBe("v2");

    s().enqueue({ tabId: "t2", path: "/b.sql", onDisk: "x" });
    expect(s().queue.map((q) => q.path)).toEqual(["/a.sql", "/b.sql"]);
  });

  it("'keep' leaves the buffer dirty and advances the queue", () => {
    const id = useEditorStore.getState().openFileTab("/a.sql", "orig");
    useEditorStore.getState().setSql(id, "my edits");
    const cs = () => useConflictStore.getState();
    cs().enqueue({ tabId: id, path: "/a.sql", onDisk: "disk v2" });
    cs().enqueue({ tabId: id, path: "/b.sql", onDisk: "other" });

    cs().resolveFront("keep");
    const tab = useEditorStore.getState().tabs.find((t) => t.id === id)!;
    expect(tab.sql).toBe("my edits"); // unchanged
    expect(cs().queue.map((q) => q.path)).toEqual(["/b.sql"]); // advanced
  });

  it("'reload' replaces the buffer + savedSql with disk content", () => {
    const id = useEditorStore.getState().openFileTab("/c.sql", "orig");
    useEditorStore.getState().setSql(id, "my edits");
    const cs = () => useConflictStore.getState();
    cs().enqueue({ tabId: id, path: "/c.sql", onDisk: "disk wins" });

    cs().resolveFront("reload");
    const tab = useEditorStore.getState().tabs.find((t) => t.id === id)!;
    expect(tab.sql).toBe("disk wins");
    expect(tab.savedSql).toBe("disk wins");
    expect(cs().queue).toHaveLength(0);
  });
});
