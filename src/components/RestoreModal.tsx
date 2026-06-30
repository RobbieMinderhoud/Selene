/**
 * Restore… dialog: restore a `.bak` from the **SQL Server host** *over* an
 * existing target database — overwriting it in place (`RESTORE … WITH REPLACE`,
 * relocating data/log files). The backup may originate from a different
 * database; the target keeps its own name.
 *
 * `RESTORE` runs server-side, so the `.bak` must be a path the server can read:
 * browse the server's folders (or type a path). This is destructive, so it
 * requires typing the target database name to confirm (like Drop). After a file
 * is chosen we preview its logical files via `restore_filelist`. Progress
 * streams over a `Channel<RestoreEvent>`.
 */

import { useEffect, useRef, useState } from "react";

import {
  backupCancel,
  databaseRestore,
  restoreFilelist,
  serverDefaultBackupDir,
} from "../ipc/commands";
import { createRestoreChannel } from "../ipc/channels";
import { asIpcError, type BackupFile } from "../ipc/types";
import { useSettingsStore } from "../state/settingsStore";
import { toastInfo, toastSuccess } from "../state/toastStore";
import { Modal } from "./Modal";
import { OperationProgress } from "./OperationProgress";
import { ServerPathBrowser } from "./ServerPathBrowser";
import styles from "./BackupRestore.module.css";

type Phase = "idle" | "running" | "error";

/** Human label for a FILELISTONLY file class. */
function fileKind(type: string): string {
  switch (type.toUpperCase()) {
    case "D":
      return "data";
    case "L":
      return "log";
    case "F":
      return "ftext";
    case "S":
      return "fstrm";
    default:
      return type;
  }
}

export function RestoreModal({
  open,
  sessionId,
  target,
  onClose,
  onRestored,
}: {
  open: boolean;
  sessionId: string;
  /** The existing database the backup will be restored over. */
  target: string;
  onClose: () => void;
  /** Called after a successful restore so the tree can refresh. */
  onRestored?: () => void;
}) {
  const defaultChecksum = useSettingsStore((s) => s.backup.checksum);

  const [path, setPath] = useState("");
  const [defaultDir, setDefaultDir] = useState("");
  const [browseOpen, setBrowseOpen] = useState(false);
  const [files, setFiles] = useState<BackupFile[] | null>(null);
  const [filesError, setFilesError] = useState<string | null>(null);
  const [checksum, setChecksum] = useState(defaultChecksum);
  const [confirmText, setConfirmText] = useState("");
  const [phase, setPhase] = useState<Phase>("idle");
  const [percent, setPercent] = useState<number | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const opId = useRef<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setPath("");
    setFiles(null);
    setFilesError(null);
    setChecksum(defaultChecksum);
    setConfirmText("");
    setPhase("idle");
    setPercent(null);
    setErrorMsg(null);
    opId.current = null;

    let cancelled = false;
    serverDefaultBackupDir(sessionId)
      .then((dir) => {
        if (!cancelled) setDefaultDir(dir);
      })
      .catch(() => {
        /* no default dir — browse from wherever the user types */
      });
    return () => {
      cancelled = true;
    };
  }, [open, defaultChecksum, sessionId]);

  const running = phase === "running";
  const confirmed = confirmText === target;
  const canRestore =
    !!path.trim() && !!files && !filesError && confirmed && !running;

  /** Preview the chosen `.bak`'s logical files (and surface a read error). */
  async function loadFilelist(p: string) {
    const trimmed = p.trim();
    setFiles(null);
    setFilesError(null);
    if (!trimmed) return;
    try {
      setFiles(await restoreFilelist(sessionId, trimmed));
    } catch (e) {
      setFilesError(asIpcError(e).message);
    }
  }

  async function runRestore() {
    const src = path.trim();
    if (!src) return;
    setPhase("running");
    setPercent(null);
    setErrorMsg(null);
    const channel = createRestoreChannel((e) => {
      if (e.kind === "started") opId.current = e.operationId;
      else if (e.kind === "progress") setPercent(e.percent);
    });
    try {
      const summary = await databaseRestore(
        sessionId,
        target,
        src,
        { checksum },
        channel,
      );
      if (summary.cancelled) {
        toastInfo("Restore cancelled");
      } else {
        toastSuccess(`Restored over "${target}"`);
        onRestored?.();
      }
      onClose();
    } catch (e) {
      setPhase("error");
      setErrorMsg(asIpcError(e).message);
    }
  }

  function cancel() {
    if (opId.current) void backupCancel(opId.current);
  }

  const footer = running ? (
    <button type="button" className="ghost" onClick={cancel}>
      Cancel restore
    </button>
  ) : (
    <>
      <button type="button" className="ghost" onClick={onClose}>
        Close
      </button>
      <button
        type="button"
        className="danger"
        disabled={!canRestore}
        onClick={() => void runRestore()}
      >
        {phase === "error" ? "Retry restore" : "Restore"}
      </button>
    </>
  );

  return (
    <Modal
      open={open}
      title={`Restore over "${target}"`}
      onClose={running ? () => undefined : onClose}
      tone="danger"
      width={480}
      footer={footer}
    >
      <div className={styles.warn}>
        <span className={styles.warnTitle}>This overwrites "{target}"</span>
        <span>
          The current contents of "{target}" are replaced by the backup. This
          cannot be undone. Other connections to it are disconnected.
        </span>
      </div>

      <div className={styles.field}>
        <span className={styles.label}>Backup file (.bak) on the server</span>
        <div className={styles.pathRow}>
          <input
            className={styles.confirmInput}
            value={path}
            spellCheck={false}
            autoComplete="off"
            aria-label="Backup file path"
            disabled={running}
            onChange={(e) => setPath(e.target.value)}
            onBlur={() => void loadFilelist(path)}
          />
          <button
            type="button"
            onClick={() => setBrowseOpen(true)}
            disabled={running}
          >
            Browse…
          </button>
        </div>
        <span className={styles.help}>
          The file is read by the SQL Server host, not your machine.
        </span>
      </div>

      {filesError && <div className={styles.errorBox}>{filesError}</div>}

      {files && files.length > 0 && (
        <div className={styles.fileList}>
          {files.map((f) => (
            <div className={styles.fileRow} key={f.logical_name}>
              <span className={styles.fileKind}>{fileKind(f.file_type)}</span>
              <span className={styles.fileName} title={f.physical_name}>
                {f.logical_name}
              </span>
            </div>
          ))}
        </div>
      )}

      <div className={styles.options}>
        <label className={styles.option}>
          <input
            type="checkbox"
            checked={checksum}
            disabled={running}
            onChange={(e) => setChecksum(e.target.checked)}
          />
          <span className={styles.optionText}>
            <span>Checksum</span>
            <span className={styles.help}>
              Verify page checksums while restoring.
            </span>
          </span>
        </label>
      </div>

      {!running && (
        <div className={styles.field}>
          <span className={styles.label}>
            Type <strong>{target}</strong> to confirm
          </span>
          <input
            className={styles.confirmInput}
            value={confirmText}
            spellCheck={false}
            autoComplete="off"
            placeholder={target}
            disabled={running || !files || !!filesError}
            onChange={(e) => setConfirmText(e.target.value)}
          />
        </div>
      )}

      {running && (
        <>
          <OperationProgress label="Restoring" percent={percent} />
          <span className={styles.help}>
            Cancelling stops the restore and may leave the database in a
            restoring state needing manual recovery.
          </span>
        </>
      )}
      {phase === "error" && errorMsg && (
        <div className={styles.errorBox}>{errorMsg}</div>
      )}

      <ServerPathBrowser
        open={browseOpen}
        sessionId={sessionId}
        mode="openBak"
        initialPath={defaultDir || "/"}
        onClose={() => setBrowseOpen(false)}
        onPick={(picked) => {
          setPath(picked);
          setBrowseOpen(false);
          void loadFilelist(picked);
        }}
      />
    </Modal>
  );
}
