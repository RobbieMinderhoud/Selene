/**
 * Windows window controls (minimize / maximize-restore / close).
 *
 * Rendered into our custom title bar only on Windows, where `decorations: false`
 * removes the native OS title bar (see `tauri.windows.conf.json`). Drives the
 * window through `@tauri-apps/api/window`; the maximize button reflects the live
 * maximized state (toggling its glyph) by polling `isMaximized()` on every
 * `onResized` event. The buttons opt out of the title bar's drag region via
 * `app-region: no-drag` in the stylesheet so clicks aren't swallowed by dragging.
 */

import { useEffect, useState } from "react";

import { getCurrentWindow } from "@tauri-apps/api/window";

import {
  CloseIcon,
  WindowMaximizeIcon,
  WindowMinimizeIcon,
  WindowRestoreIcon,
} from "./icons";
import styles from "./WindowControls.module.css";

const appWindow = getCurrentWindow();

export function WindowControls() {
  const [maximized, setMaximized] = useState(false);

  // Keep the maximize/restore glyph in sync with the real window state. A resize
  // fires whenever the window maximizes, restores, or snaps, so re-querying there
  // covers every transition (incl. the OS-driven Aero Snap ones).
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;

    const sync = () =>
      void appWindow.isMaximized().then((m) => {
        if (active) setMaximized(m);
      });

    sync();
    void appWindow
      .onResized(sync)
      .then((fn) => (active ? (unlisten = fn) : fn()));

    return () => {
      active = false;
      unlisten?.();
    };
  }, []);

  return (
    <div className={styles.controls}>
      <button
        type="button"
        className={styles.btn}
        aria-label="Minimize"
        title="Minimize"
        onClick={() => void appWindow.minimize()}
      >
        <WindowMinimizeIcon />
      </button>
      <button
        type="button"
        className={styles.btn}
        aria-label={maximized ? "Restore" : "Maximize"}
        title={maximized ? "Restore" : "Maximize"}
        onClick={() => void appWindow.toggleMaximize()}
      >
        {maximized ? <WindowRestoreIcon /> : <WindowMaximizeIcon />}
      </button>
      <button
        type="button"
        className={`${styles.btn} ${styles.close}`}
        aria-label="Close"
        title="Close"
        onClick={() => void appWindow.close()}
      >
        <CloseIcon />
      </button>
    </div>
  );
}
