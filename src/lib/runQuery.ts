/**
 * The run flow: guard -> (confirm modal) -> stream into the result store.
 *
 * This module owns the wiring between the streaming `Channel<QueryEvent>` and
 * the editor store. It is deliberately framework-light (a plain async function
 * plus a tiny confirm-bridge) so the channel callback drives store actions
 * directly — each `rows` event calls `resultAppendRows`, which mutates the
 * buffer and bumps `rev`; only the grid re-renders, never the editor.
 */

import { createQueryChannel } from "../ipc/channels";
import {
  guardCheck,
  queryCancel,
  queryRun,
  sessionCurrentDatabase,
} from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import type { DriverId, GuardVerdict, QueryEvent } from "../ipc/types";
import { getTab, useEditorStore } from "../state/editorStore";
import { useSettingsStore } from "../state/settingsStore";
import { toastError, toastInfo } from "../state/toastStore";

/** Default row cap (mirrors the backend default; kept generous but bounded). */
export const DEFAULT_MAX_ROWS = 50_000;

/**
 * Resolve the maxRows to pass to the backend.
 * Precedence: caller arg > store setting.
 */
function resolveMaxRows(callerArg: number | undefined): number {
  if (callerArg !== undefined) return callerArg;
  return useSettingsStore.getState().results.defaultRowLimit;
}

/**
 * A request to confirm a `confirm`-level guard verdict. The UI sets a single
 * resolver; `runQuery` awaits it. Returning `true` runs, `false` aborts.
 */
export type ConfirmFn = (verdict: GuardVerdict) => Promise<boolean>;

/**
 * A request to show a blocking `block` verdict (no run). The UI displays the
 * reasons; nothing to await.
 */
export type BlockFn = (verdict: GuardVerdict) => void;

export interface RunOptions {
  tabId: string;
  sessionId: string;
  sql: string;
  readOnly: boolean;
  maxRows?: number;
  /**
   * The tab's driver, threaded into the pre-run guard so a MongoDB tab is
   * classified with the Mongo guard (mongosh calls) rather than the SQL one.
   * `undefined` falls back to the SQL classifier.
   */
  driver?: DriverId;
  /** Shows the confirm modal; resolves to whether the user accepted. */
  onConfirm: ConfirmFn;
  /** Shows the block message. */
  onBlock: BlockFn;
}

/**
 * Drive one query end-to-end. Resolves once the query has been *started*
 * (the stream then continues to feed the store via the channel). Resolves early
 * (without running) if the guard blocks or the user declines a confirm.
 */
export async function runQuery(opts: RunOptions): Promise<void> {
  const { tabId, sessionId, sql, readOnly, driver } = opts;
  const store = useEditorStore.getState();

  if (!sql.trim()) return;

  // Snapshot the active database so we can notify the user if a `USE` in this
  // batch switched it (the indicator updates silently otherwise).
  const previousDatabase = getTab(tabId)?.currentDatabase ?? null;

  // (1) Guard check.
  let verdict: GuardVerdict;
  try {
    verdict = await guardCheck(sql, readOnly, driver);
  } catch (err) {
    const e = asIpcError(err);
    toastError("Guard check failed", e.message);
    return;
  }

  if (verdict.level === "block") {
    opts.onBlock(verdict);
    return;
  }
  if (verdict.level === "confirm") {
    const accepted = await opts.onConfirm(verdict);
    if (!accepted) return;
  }
  // confirmOnReadWrite: also confirm info-level queries on read-write connections.
  if (
    verdict.level === "info" &&
    !readOnly &&
    useSettingsStore.getState().query.confirmOnReadWrite
  ) {
    const accepted = await opts.onConfirm(verdict);
    if (!accepted) return;
  }

  // (2) Reset result state and open the streaming channel.
  store.resetResult(tabId);

  const channel = createQueryChannel((event: QueryEvent) => {
    const s = useEditorStore.getState();
    switch (event.kind) {
      case "started":
        s.resultStarted(tabId, event.queryId);
        break;
      case "meta":
        s.resultMeta(tabId, event.setIndex, event.columns);
        break;
      case "rows":
        s.resultAppendRows(tabId, event.setIndex, event.rows);
        break;
      case "setEnd":
        s.resultSetEnd(tabId, event.setIndex, event.affected);
        break;
      case "finished":
        s.resultFinished(tabId, event.outcome, event.elapsedMs);
        // Refresh current database — a USE statement in the batch may have changed it.
        void sessionCurrentDatabase(sessionId)
          .then((db) => {
            const newDb = db || null;
            useEditorStore.getState().setTabDatabase(tabId, newDb);
            // Surface an explicit "switched database" notice when a USE took
            // effect. Gated on a known previous value so the first query after
            // connecting (when the indicator is just being populated) is silent.
            if (newDb && previousDatabase && newDb !== previousDatabase) {
              toastInfo(`Switched to database ${newDb}`);
            }
          })
          .catch(() => {});
        break;
      case "cancelled":
        s.resultCancelled(tabId);
        break;
      case "failed":
        s.resultFailed(tabId, event.message);
        break;
    }
  });

  // (3) Kick off the run. The `started` event also carries the queryId, but we
  //     record it here too so Cancel works even before the first event lands.
  //     `resultStarted` is non-destructive and keeps the first queryId it sees,
  //     so calling it after the `started` event already fired is a safe no-op.
  try {
    const { queryId } = await queryRun(
      sessionId,
      sql,
      resolveMaxRows(opts.maxRows),
      channel,
    );
    useEditorStore.getState().resultStarted(tabId, queryId);
  } catch (err) {
    const e = asIpcError(err);
    // A server-side guard `Block` arrives here as kind "blocked".
    useEditorStore.getState().resultFailed(tabId, e.message);
    toastError("Query failed", e.message);
  }
}

/** Cancel the in-flight query for a tab (cooperative; ends with `cancelled`). */
export async function cancelQuery(tabId: string): Promise<void> {
  const result = useEditorStore.getState().results[tabId];
  if (!result?.queryId) return;
  try {
    await queryCancel(result.queryId);
  } catch (err) {
    const e = asIpcError(err);
    toastError("Cancel failed", e.message);
  }
}
