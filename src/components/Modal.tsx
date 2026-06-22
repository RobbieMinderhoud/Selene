/** A small, accessible modal dialog (overlay + centered card). */

import { useEffect, useRef } from "react";
import type { ReactNode } from "react";

import { MOTION, usePresence } from "../lib/motion";
import { CloseIcon } from "./icons";
import styles from "./Modal.module.css";

interface ModalProps {
  open: boolean;
  title: string;
  onClose: () => void;
  children: ReactNode;
  /** Footer actions (buttons), rendered right-aligned. */
  footer?: ReactNode;
  /** Tone of the header accent bar. */
  tone?: "default" | "danger" | "warning";
  /** Card width — a px number or any CSS width (e.g. `"min(960px, 94vw)"`). */
  width?: number | string;
}

export function Modal({
  open,
  title,
  onClose,
  children,
  footer,
  tone = "default",
  width = 460,
}: ModalProps) {
  const cardRef = useRef<HTMLDivElement>(null);
  // Keep the dialog mounted while it animates closed (see `usePresence`).
  const { mounted, state } = usePresence(open, MOTION.base);

  useEffect(() => {
    if (!open) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  // Focus the card only when the dialog *opens* — keyed on `open` alone so an
  // unstable `onClose` (a fresh closure each render of the parent) can't re-fire
  // this and steal focus from an input mid-typing.
  useEffect(() => {
    if (open) cardRef.current?.focus({ preventScroll: true });
  }, [open]);

  if (!mounted) return null;

  return (
    <div
      className={styles.overlay}
      data-state={state}
      role="presentation"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        ref={cardRef}
        className={styles.card}
        data-state={state}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        tabIndex={-1}
        style={{ width }}
      >
        <header className={`${styles.header} ${styles[tone]}`}>
          <h2 className={styles.title}>{title}</h2>
          <button
            type="button"
            className="ghost"
            aria-label="Close dialog"
            onClick={onClose}
          >
            <CloseIcon />
          </button>
        </header>
        <div className={styles.body}>{children}</div>
        {footer && <footer className={styles.footer}>{footer}</footer>}
      </div>
    </div>
  );
}
