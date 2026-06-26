/**
 * Multi-target store — the run-lifecycle reducers the MultiEvent channel drives
 * (started → target/targetDone/serverError → finished), plus selection helpers.
 */

import { beforeEach, describe, expect, it } from "vitest";

import { useMultiTargetStore } from "./multiTargetStore";

const TAB = "tab-1";

function view() {
  return useMultiTargetStore.getState().views[TAB];
}

beforeEach(() => {
  useMultiTargetStore.setState({ views: {} });
  useMultiTargetStore.getState().ensure(TAB, "SELECT name FROM sys.databases");
});

describe("multiTargetStore", () => {
  it("ensure seeds the filter query and is idempotent", () => {
    expect(view().filterSql).toBe("SELECT name FROM sys.databases");
    expect(view().dbMode).toBe("query");
    // A second ensure with a different seed must not clobber the existing view.
    useMultiTargetStore.getState().ensure(TAB, "OTHER");
    expect(view().filterSql).toBe("SELECT name FROM sys.databases");
  });

  it("toggleConnection adds then removes a server", () => {
    const s = useMultiTargetStore.getState();
    s.toggleConnection(TAB, "c1");
    s.toggleConnection(TAB, "c2");
    expect(view().selectedConnectionIds).toEqual(["c1", "c2"]);
    s.toggleConnection(TAB, "c1");
    expect(view().selectedConnectionIds).toEqual(["c2"]);
  });

  it("setListSelection stores per-connection database picks", () => {
    useMultiTargetStore.getState().setListSelection(TAB, "c1", ["a", "b"]);
    expect(view().listSelections.c1).toEqual(["a", "b"]);
  });

  it("startRun resets progress and marks running", () => {
    const s = useMultiTargetStore.getState();
    s.markDone(TAB, "c1", "srv", "db", 1, null); // stale row from a prior run
    s.startRun(TAB, "results", 3, "run-1");
    expect(view().runStatus).toBe("running");
    expect(view().runMode).toBe("results");
    expect(view().total).toBe(3);
    expect(view().runId).toBe("run-1");
    expect(view().progress).toEqual([]);
  });

  it("markPending then markDone upserts the same (server, db) row", () => {
    const s = useMultiTargetStore.getState();
    s.startRun(TAB, "results", 1, "run-1");
    s.markPending(TAB, "c1", "srv", "db1");
    expect(view().progress).toHaveLength(1);
    expect(view().progress[0].status).toBe("pending");
    s.markDone(TAB, "c1", "srv", "db1", 7, null);
    expect(view().progress).toHaveLength(1); // upsert, not append
    expect(view().progress[0].status).toBe("ok");
    expect(view().progress[0].rows).toBe(7);
  });

  it("markDone with an error marks the row errored", () => {
    const s = useMultiTargetStore.getState();
    s.startRun(TAB, "execute", 1, "run-1");
    s.markDone(TAB, "c1", "srv", "db1", null, "boom");
    expect(view().progress[0].status).toBe("error");
    expect(view().progress[0].error).toBe("boom");
  });

  it("markServerError records a server-level row (empty database)", () => {
    const s = useMultiTargetStore.getState();
    s.startRun(TAB, "execute", 2, "run-1");
    s.markServerError(TAB, "c1", "srv", "no stored password");
    expect(view().progress[0].database).toBe("");
    expect(view().progress[0].status).toBe("error");
  });

  it("pauseRun marks paused and stores the at-pause counts", () => {
    const s = useMultiTargetStore.getState();
    s.startRun(TAB, "execute", 50, "run-1");
    s.pauseRun(TAB, 5, 50);
    expect(view().runStatus).toBe("paused");
    expect(view().failed).toBe(5);
    expect(view().total).toBe(50);
    // The cancel handle is kept so Continue/Stop can act on the run.
    expect(view().runId).toBe("run-1");
  });

  it("resumeRun returns a paused run to running, and is a no-op otherwise", () => {
    const s = useMultiTargetStore.getState();
    s.startRun(TAB, "execute", 10, "run-1");
    s.pauseRun(TAB, 2, 10);
    s.resumeRun(TAB);
    expect(view().runStatus).toBe("running");
    // Resuming a non-paused run must not change its status.
    s.finishRun(TAB, 8, 2, 0);
    s.resumeRun(TAB);
    expect(view().runStatus).toBe("done");
  });

  it("finishRun records authoritative counts and clears runId", () => {
    const s = useMultiTargetStore.getState();
    s.startRun(TAB, "results", 5, "run-1");
    s.finishRun(TAB, 4, 1, 123);
    expect(view().runStatus).toBe("done");
    expect(view().succeeded).toBe(4);
    expect(view().failed).toBe(1);
    expect(view().rowsTotal).toBe(123);
    expect(view().runId).toBeNull();
  });

  it("remove drops the view (tab close)", () => {
    useMultiTargetStore.getState().remove(TAB);
    expect(view()).toBeUndefined();
  });

  it("actions on a removed view are a no-op (run finishing after close)", () => {
    useMultiTargetStore.getState().remove(TAB);
    // Must not throw or resurrect the view.
    useMultiTargetStore.getState().finishRun(TAB, 1, 0, 0);
    expect(view()).toBeUndefined();
  });
});
