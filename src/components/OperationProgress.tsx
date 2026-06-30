/**
 * Progress bar shared by the Back Up… / Restore… dialogs.
 *
 * `percent` is the server's `percent_complete` (0–100) when available; `null`
 * means progress could not be sampled (e.g. the polling connection lacks
 * `VIEW SERVER STATE`), so an indeterminate sliding bar is shown instead. Motion
 * uses the shared tokens and is neutralised by the global reduced-motion guard.
 */

import styles from "./BackupRestore.module.css";

export function OperationProgress({
  label,
  percent,
}: {
  /** Verb shown beside the percentage, e.g. "Backing up" / "Restoring". */
  label: string;
  percent: number | null;
}) {
  const known = percent != null;
  const clamped = known ? Math.max(0, Math.min(100, percent)) : 0;
  return (
    <div className={styles.progress}>
      <div className={styles.progressHead}>
        <span>{label}…</span>
        {known && (
          <span className={styles.progressPct}>{Math.round(clamped)}%</span>
        )}
      </div>
      <div
        className={styles.bar}
        role="progressbar"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={known ? Math.round(clamped) : undefined}
      >
        {known ? (
          <div
            className={styles.fill}
            style={{ transform: `scaleX(${clamped / 100})` }}
          />
        ) : (
          <div className={styles.fillIndeterminate} />
        )}
      </div>
    </div>
  );
}
