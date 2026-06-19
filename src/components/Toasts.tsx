/** Renders the stack of active toasts (bottom-right), driven by the toast store. */

import { useToastStore } from "../state/toastStore";

import { CloseIcon, ErrorIcon, InfoIcon, SuccessIcon } from "./icons";
import styles from "./Toasts.module.css";

export function Toasts() {
  const toasts = useToastStore((s) => s.toasts);
  const requestDismiss = useToastStore((s) => s.requestDismiss);

  if (toasts.length === 0) return null;

  return (
    <div className={styles.stack} role="region" aria-label="Notifications">
      {toasts.map((t) => (
        <div
          key={t.id}
          className={`${styles.toast} ${styles[t.kind]}`}
          data-state={t.leaving ? "closed" : "open"}
          role={t.kind === "error" ? "alert" : "status"}
        >
          {t.kind === "error" ? (
            <ErrorIcon className={styles.icon} />
          ) : t.kind === "success" ? (
            <SuccessIcon className={styles.icon} />
          ) : (
            <InfoIcon className={styles.icon} />
          )}
          <div className={styles.content}>
            <span className={styles.message}>{t.message}</span>
            {t.detail && <span className={styles.detail}>{t.detail}</span>}
          </div>
          <button
            type="button"
            className="ghost"
            aria-label="Dismiss notification"
            onClick={() => requestDismiss(t.id)}
          >
            <CloseIcon />
          </button>
        </div>
      ))}
    </div>
  );
}
