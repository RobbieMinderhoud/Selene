/**
 * Result-reducer lifecycle for the editor store.
 *
 * These reducers are the heart of the streaming pipeline: the query channel
 * drives them on every event, and the grid repaints off the `rev` counter and
 * the in-place-mutated row buffers. We exercise a full lifecycle plus the
 * terminal branches and the multi-result-set indexing, all as pure store calls
 * (no React, no IPC).
 */

import { beforeEach, describe, expect, it } from "vitest";

import type { CellValue, Column, ExecOutcome } from "../ipc/types";
import { selectDirty, useEditorStore } from "./editorStore";

/** A two-column metadata fixture. */
function cols(...names: string[]): Column[] {
  return names.map((name, i) => ({
    name,
    ordinal: i,
    db_type: "int",
    logical: "integer",
    nullable: false,
  }));
}

/** A batch of rows, each a single I64 cell carrying `n`. */
function rows(...values: number[]): CellValue[][] {
  return values.map((v) => [{ t: "I64", v }] as CellValue[]);
}

const outcome = (over: Partial<ExecOutcome> = {}): ExecOutcome => ({
  result_sets: 1,
  total_rows: 0,
  truncated: false,
  rolled_back: false,
  ...over,
});

/** Add a tab and return its id and a fresh getState() each call. */
function freshTab(): string {
  return useEditorStore.getState().addTab(null, "");
}

function result(id: string) {
  const r = useEditorStore.getState().results[id];
  if (!r) throw new Error(`no result state for ${id}`);
  return r;
}

beforeEach(() => {
  // Reset the singleton store between tests.
  useEditorStore.setState({ tabs: [], activeTabId: null, results: {} });
});

describe("editorStore result reducers", () => {
  it("drives a full single-set lifecycle: started -> meta -> rows -> setEnd -> finished", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();

    expect(result(id).status).toBe("idle");

    s().resultStarted(id, "q-1");
    expect(result(id).status).toBe("running");
    expect(result(id).queryId).toBe("q-1");

    s().resultMeta(id, 0, cols("a"));
    expect(result(id).resultSets).toHaveLength(1);
    expect(result(id).resultSets[0].columns.map((c) => c.name)).toEqual(["a"]);

    s().resultAppendRows(id, 0, rows(1, 2));
    s().resultAppendRows(id, 0, rows(3));
    // Rows accumulate across batches, in arrival order.
    expect(result(id).resultSets[0].rows).toHaveLength(3);
    expect(result(id).resultSets[0].rows.map((r) => r[0])).toEqual([
      { t: "I64", v: 1 },
      { t: "I64", v: 2 },
      { t: "I64", v: 3 },
    ]);
    expect(result(id).rowCount).toBe(3);

    s().resultSetEnd(id, 0, null);
    expect(result(id).resultSets[0].affected).toBeNull();

    s().resultFinished(id, outcome({ total_rows: 3 }), 42);
    expect(result(id).status).toBe("done");
    expect(result(id).queryId).toBeNull();
    expect(result(id).elapsedMs).toBe(42);
    expect(result(id).rowCount).toBe(3);
  });

  it("bumps the grid revision counter on every mutation", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();
    const start = result(id).rev;

    s().resultStarted(id, "q-1");
    s().resultMeta(id, 0, cols("a"));
    s().resultAppendRows(id, 0, rows(1));
    s().resultAppendRows(id, 0, rows(2));
    s().resultSetEnd(id, 0, null);
    s().resultFinished(id, outcome(), 1);

    // Six mutations -> six bumps. The grid subscribes to this to repaint.
    expect(result(id).rev).toBe(start + 6);
  });

  it("bumps rev specifically on each row append (grid repaint signal)", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();
    s().resultMeta(id, 0, cols("a"));

    const before = result(id).rev;
    s().resultAppendRows(id, 0, rows(1));
    expect(result(id).rev).toBe(before + 1);
    s().resultAppendRows(id, 0, rows(2, 3));
    expect(result(id).rev).toBe(before + 2);
  });

  it("keeps multiple result sets separate, indexed by setIndex", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();

    s().resultStarted(id, "q-1");
    s().resultMeta(id, 0, cols("first"));
    s().resultAppendRows(id, 0, rows(10, 11));
    s().resultSetEnd(id, 0, null);

    s().resultMeta(id, 1, cols("second_a", "second_b"));
    s().resultAppendRows(id, 1, rows(20));
    s().resultSetEnd(id, 1, 7);

    const sets = result(id).resultSets;
    expect(sets).toHaveLength(2);

    const set0 = sets.find((r) => r.setIndex === 0)!;
    const set1 = sets.find((r) => r.setIndex === 1)!;

    expect(set0.columns.map((c) => c.name)).toEqual(["first"]);
    expect(set0.rows).toHaveLength(2);
    expect(set0.affected).toBeNull();

    expect(set1.columns.map((c) => c.name)).toEqual(["second_a", "second_b"]);
    expect(set1.rows).toHaveLength(1);
    expect(set1.affected).toBe(7);

    // Total rows is across all sets.
    expect(result(id).rowCount).toBe(3);
  });

  it("records affected-rows for a DML set via setEnd", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();
    s().resultMeta(id, 0, cols("x"));
    s().resultSetEnd(id, 0, 12);
    expect(result(id).resultSets[0].affected).toBe(12);
  });

  it("resetResult clears rows, columns, status, error and assigns a new runId", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();

    s().resultStarted(id, "q-1");
    s().resultMeta(id, 0, cols("a"));
    s().resultAppendRows(id, 0, rows(1, 2, 3));
    s().resultFailed(id, "boom");

    const beforeRunId = result(id).runId;

    s().resetResult(id);
    const r = result(id);
    expect(r.status).toBe("idle");
    expect(r.resultSets).toEqual([]);
    expect(r.rowCount).toBe(0);
    expect(r.error).toBeNull();
    expect(r.queryId).toBeNull();
    expect(r.elapsedMs).toBeNull();
    // A new run gets a fresh runId so the grid virtualizer resets.
    expect(r.runId).not.toBe(beforeRunId);
  });

  it("transitions to cancelled and clears the queryId on resultCancelled", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();
    s().resultStarted(id, "q-1");
    s().resultAppendRows(id, 0, rows(1));

    s().resultCancelled(id);
    expect(result(id).status).toBe("cancelled");
    expect(result(id).queryId).toBeNull();
    // Already-buffered rows survive cancellation.
    expect(result(id).resultSets[0]?.rows ?? []).toHaveLength(1);
  });

  it("transitions to failed with the message on resultFailed", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();
    s().resultStarted(id, "q-1");

    s().resultFailed(id, "syntax error near 'FROM'");
    expect(result(id).status).toBe("failed");
    expect(result(id).error).toBe("syntax error near 'FROM'");
    expect(result(id).queryId).toBeNull();
  });

  it("resultStarted keeps the first queryId it sees (idempotent re-call)", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();
    s().resultStarted(id, "first-id");
    // runQuery may call resultStarted again with the awaited id; the first wins
    // so an early `started` event's id is never clobbered.
    s().resultStarted(id, "second-id");
    expect(result(id).queryId).toBe("first-id");
    expect(result(id).status).toBe("running");
  });

  it("propagates the truncated flag and authoritative total_rows from finished", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();
    s().resultMeta(id, 0, cols("a"));
    s().resultAppendRows(id, 0, rows(1, 2));

    s().resultFinished(id, outcome({ truncated: true, total_rows: 50000 }), 99);
    expect(result(id).truncated).toBe(true);
    // Server count overrides the locally-tallied count.
    expect(result(id).rowCount).toBe(50000);
  });

  it("propagates the rolled_back flag from finished", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();
    // A rollback-wrapped DML dry-run: one column-less set carrying its count.
    s().resultMeta(id, 0, []);
    s().resultSetEnd(id, 0, 193);
    expect(result(id).rolledBack).toBe(false);

    s().resultFinished(id, outcome({ rolled_back: true }), 12);
    expect(result(id).rolledBack).toBe(true);
    expect(result(id).resultSets[0].affected).toBe(193);
  });

  it("stays robust if rows arrive before meta (synthesizes a set)", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();
    s().resultAppendRows(id, 0, rows(1, 2));
    const set0 = result(id).resultSets.find((r) => r.setIndex === 0)!;
    expect(set0.rows).toHaveLength(2);
    expect(set0.columns).toEqual([]);
  });

  it("setActiveSet switches the active result-set sub-tab and bumps rev", () => {
    const id = freshTab();
    const s = () => useEditorStore.getState();
    s().resultMeta(id, 0, cols("a"));
    s().resultMeta(id, 1, cols("b"));
    const before = result(id).rev;

    s().setActiveSet(id, 1);
    expect(result(id).activeSet).toBe(1);
    expect(result(id).rev).toBe(before + 1);
  });
});

describe("editorStore file-backed tabs", () => {
  const s = () => useEditorStore.getState();
  const tab = (id: string) => s().tabs.find((t) => t.id === id)!;

  it("openFileTab creates a clean file-backed tab titled with the basename", () => {
    const id = s().openFileTab("/work/project/report.sql", "SELECT 1;");
    const t = tab(id);
    expect(t.title).toBe("report.sql");
    expect(t.filePath).toBe("/work/project/report.sql");
    expect(t.sql).toBe("SELECT 1;");
    expect(t.savedSql).toBe("SELECT 1;");
    expect(t.fileMissing).toBe(false);
    expect(s().activeTabId).toBe(id);
    // A fresh file tab is not dirty (buffer matches disk).
    expect(selectDirty(s(), id)).toBe(false);
  });

  it("openFileTab is idempotent on path: focuses without clobbering edits", () => {
    const id = s().openFileTab("/work/a.sql", "original");
    // Simulate an unsaved edit, then switch away.
    s().setSql(id, "edited locally");
    const other = s().addTab(null, "");
    expect(s().activeTabId).toBe(other);

    const again = s().openFileTab("/work/a.sql", "original");
    expect(again).toBe(id); // same tab, not a duplicate
    expect(s().tabs.filter((t) => t.filePath === "/work/a.sql")).toHaveLength(
      1,
    );
    expect(s().activeTabId).toBe(id); // focused
    expect(tab(id).sql).toBe("edited locally"); // buffer preserved
  });

  it("inherits a session id when one is passed (file opened during a session)", () => {
    const id = s().openFileTab("/work/b.sql", "SELECT 2;", "sess-1");
    expect(tab(id).sessionId).toBe("sess-1");
  });

  it("markTabSaved binds path, title, savedSql and clears dirty (incl. Save As)", () => {
    const id = s().addTab(null, "SELECT 1;");
    expect(tab(id).filePath).toBeNull();

    // First save (scratch -> file).
    s().markTabSaved(id, "/work/q.sql", "SELECT 1;");
    expect(tab(id).filePath).toBe("/work/q.sql");
    expect(tab(id).title).toBe("q.sql");
    expect(selectDirty(s(), id)).toBe(false);

    // Edit then Save As to a new path -> title + path follow the new file.
    s().setSql(id, "SELECT 2;");
    expect(selectDirty(s(), id)).toBe(true);
    s().markTabSaved(id, "/work/renamed.sql", "SELECT 2;");
    expect(tab(id).filePath).toBe("/work/renamed.sql");
    expect(tab(id).title).toBe("renamed.sql");
    expect(selectDirty(s(), id)).toBe(false);
  });

  it("reloadTabFromDisk replaces buffer + savedSql and clears fileMissing", () => {
    const id = s().openFileTab("/work/c.sql", "v1");
    s().setSql(id, "local edits");
    s().setTabFileMissing(id, true);

    s().reloadTabFromDisk(id, "v2 from disk");
    expect(tab(id).sql).toBe("v2 from disk");
    expect(tab(id).savedSql).toBe("v2 from disk");
    expect(tab(id).fileMissing).toBe(false);
    expect(selectDirty(s(), id)).toBe(false);
  });

  it("selectDirty: scratch tabs are never dirty; file tabs dirty when buffer differs", () => {
    const scratch = s().addTab(null, "anything typed");
    expect(selectDirty(s(), scratch)).toBe(false);

    const file = s().openFileTab("/work/d.sql", "SELECT 1;");
    expect(selectDirty(s(), file)).toBe(false);
    s().setSql(file, "SELECT 1; -- changed");
    expect(selectDirty(s(), file)).toBe(true);
    s().setSql(file, "SELECT 1;"); // typed back to the saved content
    expect(selectDirty(s(), file)).toBe(false);

    expect(selectDirty(s(), null)).toBe(false);
  });
});
