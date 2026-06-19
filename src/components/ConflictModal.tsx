/**
 * Resolves an external-edit conflict: the file on disk changed while the tab had
 * unsaved edits. Driven by the file-sync conflict queue ({@link useConflictStore});
 * shows the front conflict and never clobbers edits without the user's choice.
 *
 * Closing via Escape/overlay/✕ is treated as "Keep my version" — the safe
 * default that preserves the user's unsaved work.
 */

import { useRef } from "react";

import { useConflictStore } from "../lib/fsSync";
import { basename } from "../lib/path";
import { Modal } from "./Modal";
import styles from "./ConflictModal.module.css";

export function ConflictModal() {
  const front = useConflictStore((s) => s.queue[0] ?? null);
  const resolveFront = useConflictStore((s) => s.resolveFront);

  // Retain the last conflict so the Modal can animate out after the queue
  // empties (otherwise it would unmount instantly). Safe during render: a pure
  // function of the prop, mirroring GuardModal.
  const last = useRef(front);
  if (front) last.current = front;
  const shown = front ?? last.current;
  if (!shown) return null;

  return (
    <Modal
      open={front !== null}
      title="File changed on disk"
      tone="warning"
      onClose={() => resolveFront("keep")}
      footer={
        <>
          <button type="button" onClick={() => resolveFront("keep")}>
            Keep my version
          </button>
          <button
            type="button"
            className="danger"
            onClick={() => resolveFront("reload")}
          >
            Reload from disk
          </button>
        </>
      }
    >
      <p className={styles.lead}>
        <strong>{basename(shown.path)}</strong> changed on disk while you had
        unsaved edits open in Selene.
      </p>
      <p className={styles.hint}>
        “Reload from disk” discards your edits and loads the new content. “Keep
        my version” keeps your edits — your next save overwrites the file.
      </p>
    </Modal>
  );
}
