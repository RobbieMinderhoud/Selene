/**
 * Browse the **SQL Server host's** filesystem to pick a backup destination
 * (`saveBak`) or an existing `.bak` to restore (`openBak`).
 *
 * `BACKUP`/`RESTORE` read & write on the server, so the path must be one the
 * server process can reach — not a path on this Mac. Directory listings come
 * from `server_list_dir` (the driver's `xp_dirtree`); the component tracks the
 * absolute path and joins with the server's separator (`\` on Windows, else
 * `/`). For a local Docker server, a folder that is bind-mounted to the host
 * (e.g. `/mnt/backups`) makes the resulting file appear on this machine.
 */

import { useEffect, useState } from "react";

import { serverListDir } from "../ipc/commands";
import { asIpcError, type ServerDirEntry } from "../ipc/types";
import { joinServerPath, parentPath } from "../lib/serverPath";
import { FileIcon, FolderIcon } from "./icons";
import { Modal } from "./Modal";
import styles from "./BackupRestore.module.css";

type Mode = "saveBak" | "openBak";

function isBak(name: string): boolean {
  return name.toLowerCase().endsWith(".bak");
}

export function ServerPathBrowser({
  open,
  sessionId,
  mode,
  initialPath,
  defaultFileName,
  onClose,
  onPick,
}: {
  open: boolean;
  sessionId: string;
  mode: Mode;
  /** Directory to start browsing in. */
  initialPath: string;
  /** Pre-filled file name for `saveBak`. */
  defaultFileName?: string;
  onClose: () => void;
  /** Called with the chosen absolute server path. */
  onPick: (path: string) => void;
}) {
  const [path, setPath] = useState(initialPath);
  const [entries, setEntries] = useState<ServerDirEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [fileName, setFileName] = useState(defaultFileName ?? "");
  const [selected, setSelected] = useState<string | null>(null);

  // Reset to the starting point each time the browser opens.
  useEffect(() => {
    if (!open) return;
    setPath(initialPath);
    setFileName(defaultFileName ?? "");
    setSelected(null);
    setError(null);
  }, [open, initialPath, defaultFileName]);

  // List whenever the directory changes (while open).
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    setSelected(null);
    serverListDir(sessionId, path)
      .then((list) => {
        if (cancelled) return;
        // Directories first, then files; alphabetical within each.
        const sorted = [...list].sort((a, b) =>
          a.is_dir === b.is_dir
            ? a.name.localeCompare(b.name)
            : a.is_dir
              ? -1
              : 1,
        );
        setEntries(sorted);
      })
      .catch((e) => {
        if (!cancelled) setError(asIpcError(e).message);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [open, sessionId, path]);

  function enter(entry: ServerDirEntry) {
    if (entry.is_dir) {
      setPath((p) => joinServerPath(p, entry.name));
    } else if (mode === "openBak" && isBak(entry.name)) {
      setSelected(entry.name);
    }
  }

  const canConfirm =
    mode === "saveBak"
      ? fileName.trim().length > 0
      : selected != null && isBak(selected);

  function confirm() {
    if (mode === "saveBak") {
      const name = fileName.trim();
      if (!name) return;
      onPick(joinServerPath(path, isBak(name) ? name : `${name}.bak`));
    } else if (selected) {
      onPick(joinServerPath(path, selected));
    }
  }

  const footer = (
    <>
      <button type="button" className="ghost" onClick={onClose}>
        Cancel
      </button>
      <button
        type="button"
        className="primary"
        disabled={!canConfirm}
        onClick={confirm}
      >
        {mode === "saveBak" ? "Use this folder" : "Select"}
      </button>
    </>
  );

  return (
    <Modal
      open={open}
      title={mode === "saveBak" ? "Choose server folder" : "Choose backup file"}
      onClose={onClose}
      width={520}
      footer={footer}
    >
      <div className={styles.field}>
        <span className={styles.label}>Folder on the server</span>
        <div className={styles.pathRow}>
          <input
            className={styles.confirmInput}
            value={path}
            spellCheck={false}
            autoComplete="off"
            aria-label="Server path"
            onChange={(e) => setPath(e.target.value)}
          />
          <button
            type="button"
            onClick={() => setPath((p) => parentPath(p))}
            title="Up one folder"
          >
            Up
          </button>
        </div>
      </div>

      {error && <div className={styles.errorBox}>{error}</div>}

      <div className={styles.browseList} aria-busy={loading}>
        {loading && <div className={styles.browseEmpty}>Loading…</div>}
        {!loading && entries.length === 0 && !error && (
          <div className={styles.browseEmpty}>Empty folder</div>
        )}
        {!loading &&
          entries.map((entry) => {
            const selectable =
              entry.is_dir || (mode === "openBak" && isBak(entry.name));
            return (
              <button
                key={entry.name}
                type="button"
                className={`${styles.browseRow} ${
                  selected === entry.name ? styles.browseSelected : ""
                }`}
                data-disabled={!selectable}
                disabled={!selectable}
                onClick={() => enter(entry)}
                onDoubleClick={() => {
                  if (
                    !entry.is_dir &&
                    mode === "openBak" &&
                    isBak(entry.name)
                  ) {
                    setSelected(entry.name);
                    confirm();
                  }
                }}
              >
                <span className={styles.browseIcon} aria-hidden>
                  {entry.is_dir ? <FolderIcon /> : <FileIcon />}
                </span>
                <span className={styles.browseName}>{entry.name}</span>
              </button>
            );
          })}
      </div>

      {mode === "saveBak" && (
        <div className={styles.field}>
          <span className={styles.label}>File name</span>
          <input
            className={styles.confirmInput}
            value={fileName}
            spellCheck={false}
            autoComplete="off"
            aria-label="Backup file name"
            onChange={(e) => setFileName(e.target.value)}
          />
        </div>
      )}
    </Modal>
  );
}
