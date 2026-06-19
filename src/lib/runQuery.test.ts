/**
 * Orchestration tests for the run flow (`runQuery`).
 *
 * The streaming `Channel` is mocked at the `../ipc/channels` boundary:
 * `createQueryChannel(onEvent)` captures `onEvent` into `capturedOnEvent` and
 * returns a sentinel, so a test can drive the store by calling
 * `emit({ kind: ... })` exactly as the Rust side would `send` over the channel.
 * All IPC commands and the toast store are mocked too — no Tauri, no network.
 */

import { beforeEach, describe, expect, it, vi } from "vitest";

import type { QueryEvent } from "../ipc/types";

// --- Mocks (declared before importing the unit under test) -----------------

vi.mock("../ipc/commands", () => ({
  guardCheck: vi.fn(),
  queryRun: vi.fn(),
  queryCancel: vi.fn(),
  // runQuery refreshes the current database after a batch finishes; stub it so
  // the post-finish `.then(...)` resolves cleanly in these store-only tests.
  sessionCurrentDatabase: vi.fn(() => Promise.resolve(null)),
}));

// The channel mock captures the event callback so the test can emit events.
let capturedOnEvent: ((event: QueryEvent) => void) | null = null;
const CHANNEL_SENTINEL = { __channel: true };
vi.mock("../ipc/channels", () => ({
  createQueryChannel: vi.fn((onEvent: (event: QueryEvent) => void) => {
    capturedOnEvent = onEvent;
    return CHANNEL_SENTINEL;
  }),
}));

vi.mock("../state/toastStore", () => ({
  toastError: vi.fn(),
  toastSuccess: vi.fn(),
}));

import { createQueryChannel } from "../ipc/channels";
import { guardCheck, queryCancel, queryRun } from "../ipc/commands";
import type { IpcError } from "../ipc/types";
import { useEditorStore } from "../state/editorStore";
import { toastError } from "../state/toastStore";
import { cancelQuery, DEFAULT_MAX_ROWS, runQuery } from "./runQuery";

const mockGuard = vi.mocked(guardCheck);
const mockRun = vi.mocked(queryRun);
const mockCancel = vi.mocked(queryCancel);
const mockToastError = vi.mocked(toastError);

/** Push an event through the captured channel callback. */
function emit(event: QueryEvent) {
  if (!capturedOnEvent) throw new Error("channel callback not captured yet");
  capturedOnEvent(event);
}

function result(id: string) {
  return useEditorStore.getState().results[id];
}

/** Standard run options with stub confirm/block handlers. */
function opts(over: Partial<Parameters<typeof runQuery>[0]> = {}) {
  return {
    tabId: "tab-1",
    sessionId: "session-1",
    sql: "SELECT 1",
    readOnly: false,
    onConfirm: vi.fn(async () => true),
    onBlock: vi.fn(),
    ...over,
  };
}

beforeEach(() => {
  capturedOnEvent = null;
  useEditorStore.setState({ tabs: [], activeTabId: null, results: {} });
  // Seed an empty result slot for tab-1 so reducers have somewhere to write.
  useEditorStore.getState().resetResult("tab-1");
  // `clearMocks` wipes call history but not implementations; reset both to
  // safe defaults so an implementation set in one test never leaks into the
  // next. Individual tests override these as needed.
  mockGuard.mockReset();
  mockRun.mockReset();
  mockCancel.mockReset();
  mockToastError.mockReset();
});

describe("runQuery — guard gating", () => {
  it("does nothing for empty / whitespace SQL", async () => {
    await runQuery(opts({ sql: "   \n\t " }));
    expect(mockGuard).not.toHaveBeenCalled();
    expect(mockRun).not.toHaveBeenCalled();
  });

  it("on a block verdict: calls onBlock and does NOT run the query", async () => {
    mockGuard.mockResolvedValue({
      level: "block",
      reasons: ["non-SELECT on a read-only connection"],
    });
    const o = opts();

    await runQuery(o);

    expect(o.onBlock).toHaveBeenCalledTimes(1);
    expect(o.onBlock).toHaveBeenCalledWith({
      level: "block",
      reasons: ["non-SELECT on a read-only connection"],
    });
    expect(o.onConfirm).not.toHaveBeenCalled();
    expect(mockRun).not.toHaveBeenCalled();
  });

  it("on a confirm verdict the user declines: does NOT run", async () => {
    mockGuard.mockResolvedValue({ level: "confirm", reasons: ["DELETE"] });
    const o = opts({ onConfirm: vi.fn(async () => false) });

    await runQuery(o);

    expect(o.onConfirm).toHaveBeenCalledTimes(1);
    expect(mockRun).not.toHaveBeenCalled();
  });

  it("on a confirm verdict the user accepts: runs once with sql/session/maxRows", async () => {
    mockGuard.mockResolvedValue({ level: "confirm", reasons: ["DELETE"] });
    mockRun.mockResolvedValue({ queryId: "q-42" });
    const o = opts({ onConfirm: vi.fn(async () => true), maxRows: 1000 });

    await runQuery(o);

    expect(o.onConfirm).toHaveBeenCalledTimes(1);
    expect(mockRun).toHaveBeenCalledTimes(1);
    expect(mockRun).toHaveBeenCalledWith(
      "session-1",
      "SELECT 1",
      1000,
      CHANNEL_SENTINEL,
    );
  });

  it("on an info verdict: runs without prompting", async () => {
    mockGuard.mockResolvedValue({ level: "info", reasons: [] });
    mockRun.mockResolvedValue({ queryId: "q-1" });
    const o = opts();

    await runQuery(o);

    expect(o.onConfirm).not.toHaveBeenCalled();
    expect(o.onBlock).not.toHaveBeenCalled();
    expect(mockRun).toHaveBeenCalledTimes(1);
  });

  it("defaults maxRows to DEFAULT_MAX_ROWS when not provided", async () => {
    mockGuard.mockResolvedValue({ level: "info", reasons: [] });
    mockRun.mockResolvedValue({ queryId: "q-1" });

    await runQuery(opts());

    expect(mockRun).toHaveBeenCalledWith(
      "session-1",
      "SELECT 1",
      DEFAULT_MAX_ROWS,
      CHANNEL_SENTINEL,
    );
  });

  it("guard_check rejection surfaces a toast and skips the run", async () => {
    mockGuard.mockRejectedValue({
      message: "guard exploded",
      kind: "guard",
    } satisfies IpcError);

    await runQuery(opts());

    expect(mockToastError).toHaveBeenCalledWith(
      "Guard check failed",
      "guard exploded",
    );
    expect(mockRun).not.toHaveBeenCalled();
  });
});

describe("runQuery — streaming into the store", () => {
  beforeEach(() => {
    mockGuard.mockResolvedValue({ level: "info", reasons: [] });
  });

  it("resets the result and wires the channel before running", async () => {
    mockRun.mockResolvedValue({ queryId: "q-1" });
    await runQuery(opts());
    expect(vi.mocked(createQueryChannel)).toHaveBeenCalledTimes(1);
    expect(capturedOnEvent).toBeTypeOf("function");
  });

  it("reflects started -> meta -> rows -> finished pushed over the channel", async () => {
    mockRun.mockResolvedValue({ queryId: "q-7" });

    await runQuery(opts());

    emit({ kind: "started", queryId: "q-7" });
    expect(result("tab-1").status).toBe("running");
    expect(result("tab-1").queryId).toBe("q-7");

    emit({
      kind: "meta",
      setIndex: 0,
      columns: [
        {
          name: "id",
          ordinal: 0,
          db_type: "int",
          logical: "integer",
          nullable: false,
        },
      ],
    });
    emit({ kind: "rows", setIndex: 0, rows: [[{ t: "I64", v: 1 }]] });
    emit({ kind: "rows", setIndex: 0, rows: [[{ t: "I64", v: 2 }]] });
    emit({ kind: "setEnd", setIndex: 0, affected: null });

    expect(result("tab-1").resultSets[0].rows).toHaveLength(2);

    emit({
      kind: "finished",
      outcome: {
        result_sets: 1,
        total_rows: 2,
        truncated: false,
        rolled_back: false,
      },
      elapsedMs: 12,
    });

    expect(result("tab-1").status).toBe("done");
    expect(result("tab-1").rowCount).toBe(2);
    expect(result("tab-1").elapsedMs).toBe(12);
  });

  it("a cancelled event drives the store to cancelled", async () => {
    mockRun.mockResolvedValue({ queryId: "q-1" });
    await runQuery(opts());
    emit({ kind: "started", queryId: "q-1" });
    emit({ kind: "cancelled" });
    expect(result("tab-1").status).toBe("cancelled");
  });

  it("a failed event drives the store to failed with the message", async () => {
    mockRun.mockResolvedValue({ queryId: "q-1" });
    await runQuery(opts());
    emit({ kind: "started", queryId: "q-1" });
    emit({ kind: "failed", message: "stream broke" });
    expect(result("tab-1").status).toBe("failed");
    expect(result("tab-1").error).toBe("stream broke");
  });

  it("query_run rejection: store goes to failed and an error toast is shown", async () => {
    mockRun.mockRejectedValue({
      message: "blocked server-side",
      kind: "blocked",
    } satisfies IpcError);

    await runQuery(opts());

    expect(result("tab-1").status).toBe("failed");
    expect(result("tab-1").error).toBe("blocked server-side");
    expect(mockToastError).toHaveBeenCalledWith(
      "Query failed",
      "blocked server-side",
    );
  });

  it("records the awaited queryId so Cancel works even before the started event", async () => {
    mockRun.mockResolvedValue({ queryId: "q-late" });
    await runQuery(opts());
    // No `started` event emitted; the awaited id was still recorded.
    expect(result("tab-1").queryId).toBe("q-late");
    expect(result("tab-1").status).toBe("running");
  });
});

describe("cancelQuery", () => {
  it("calls queryCancel with the in-flight queryId", async () => {
    mockCancel.mockResolvedValue(undefined);
    useEditorStore.getState().resultStarted("tab-1", "q-99");

    await cancelQuery("tab-1");
    expect(mockCancel).toHaveBeenCalledWith("q-99");
  });

  it("is a no-op when there is no in-flight query", async () => {
    await cancelQuery("tab-1");
    expect(mockCancel).not.toHaveBeenCalled();
  });

  it("surfaces a toast if cancellation rejects", async () => {
    mockCancel.mockRejectedValue({ message: "cannot cancel", kind: "query" });
    useEditorStore.getState().resultStarted("tab-1", "q-99");

    await cancelQuery("tab-1");
    expect(mockToastError).toHaveBeenCalledWith(
      "Cancel failed",
      "cannot cancel",
    );
  });
});
