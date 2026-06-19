/**
 * A small cursor-positioned context menu (backdrop + menu card), animated in and
 * out via `usePresence` + `data-state` like the rest of the UI. Reused by the
 * schema tree for its right-click actions.
 */

import { useEffect } from "react";

import { MOTION, usePresence } from "../lib/motion";
import styles from "./ContextMenu.module.css";

export interface ContextMenuItem {
  label: string;
  onSelect: () => void;
  disabled?: boolean;
}

export function ContextMenu({
  open,
  x,
  y,
  items,
  onClose,
}: {
  open: boolean;
  x: number;
  y: number;
  items: ContextMenuItem[];
  onClose: () => void;
}) {
  // Keep mounted across the (fast) exit animation.
  const { mounted, state } = usePresence(open, MOTION.fast);

  useEffect(() => {
    if (!open) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!mounted) return null;

  return (
    <div
      className={styles.backdrop}
      role="presentation"
      onMouseDown={onClose}
      onContextMenu={(e) => {
        e.preventDefault();
        onClose();
      }}
    >
      <div
        className={styles.menu}
        data-state={state}
        role="menu"
        style={{ left: x, top: y }}
        // Don't let a click inside the menu hit the backdrop's dismiss.
        onMouseDown={(e) => e.stopPropagation()}
      >
        {items.map((item) => (
          <button
            key={item.label}
            type="button"
            role="menuitem"
            className={styles.item}
            disabled={item.disabled}
            onClick={() => {
              onClose();
              item.onSelect();
            }}
          >
            {item.label}
          </button>
        ))}
      </div>
    </div>
  );
}
