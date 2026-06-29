/**
 * A small cursor-positioned context menu (backdrop + menu card), animated in and
 * out via `usePresence` + `data-state` like the rest of the UI. Reused by the
 * schema tree and the results grid for their right-click actions.
 *
 * An item with `children` becomes a parent row that reveals a flyout submenu on
 * hover (or click) — see {@link SubmenuItem}.
 */

import { useEffect, useState } from "react";

import { MOTION, usePresence } from "../lib/motion";
import styles from "./ContextMenu.module.css";

export interface ContextMenuItem {
  label: string;
  onSelect: () => void;
  disabled?: boolean;
  /** When present (and non-empty), this item opens a flyout submenu instead of
   *  acting on click. */
  children?: ContextMenuItem[];
}

/** A parent row whose submenu opens to the right on hover/click. */
function SubmenuItem({
  item,
  onClose,
}: {
  item: ContextMenuItem;
  onClose: () => void;
}) {
  const [open, setOpen] = useState(false);
  // Keep the submenu mounted across its (fast) exit animation.
  const { mounted, state } = usePresence(open, MOTION.fast);
  const children = item.children ?? [];

  return (
    <div
      className={styles.subWrap}
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
    >
      <button
        type="button"
        role="menuitem"
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={item.disabled}
        className={`${styles.item} ${styles.parentItem}`}
        // Click opens the submenu too (touch / click users); it never selects.
        onClick={() => setOpen(true)}
      >
        <span>{item.label}</span>
        <span className={styles.chevron} aria-hidden="true">
          ›
        </span>
      </button>
      {mounted && (
        <div className={styles.submenu} data-state={state} role="menu">
          {children.map((child) => (
            <button
              key={child.label}
              type="button"
              role="menuitem"
              className={styles.item}
              disabled={child.disabled}
              onClick={() => {
                onClose();
                child.onSelect();
              }}
            >
              {child.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
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
        {items.map((item) =>
          item.children && item.children.length > 0 ? (
            <SubmenuItem key={item.label} item={item} onClose={onClose} />
          ) : (
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
          ),
        )}
      </div>
    </div>
  );
}
