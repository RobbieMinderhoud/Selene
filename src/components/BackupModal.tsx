/**
 * Back Up… dialog: `BACKUP DATABASE` to a `.bak` file chosen via the native save
 * dialog. The chosen path is interpreted by the **SQL Server host** (the backup
 * runs server-side), so for a containerised local server the directory must be
 * reachable by the server process.
 *
 * Options seed from the Database-backup settings and are editable per run.
 * Progress (a real percentage when the server reports it) streams over a
 * `Channel<BackupEvent>` into an in-modal bar; the operation can be cancelled.
 */

import { useEffect, useRef, useState } from "react";

import { backupCancel, databaseBackup } from "../ipc/commands";
import { createBackupChannel } from "../ipc/channels";
import { asIpcError } from "../ipc/types";
import { useSettingsStore } from "../state/settingsStore";
import { toastError, toastInfo, toastSuccess } from "../state/toastStore";
import { Modal } from "./Modal";
import { OperationProgress } from "./OperationProgress";
import styles from "./BackupRestore.module.css";

const BAK_FILTER = [{ name: "SQL Server backup", extensions: ["bak"] }];

/** `YYYY-MM-DD-HHmmss` for a default backup file name. */
function timestamp(): string {
  return new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
}

type Phase = "idle" | "running" | "error";

export function BackupModal({
  open,
  sessionId,
  database,
  onClose,
}: {
  open: boolean;
  sessionId: string;
  database: string;
  onClose: () => void;
}) {
  const defaults = useSettingsStore((s) => s.backup);

  const [compression, setCompression] = useState(defaults.compression);
  const [checksum, setChecksum] = useState(defaults.checksum);
  const [verifyAfter, setVerifyAfter] = useState(defaults.verifyAfter);
  const [path, setPath] = useState<string | null>(null);
  const [phase, setPhase] = useState<Phase>("idle");
  const [percent, setPercent] = useState<number | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const opId = useRef<string | null>(null);

  // Reset to current settings each time the dialog opens.
  useEffect(() => {
    if (!open) return;
    setCompression(defaults.compression);
    setChecksum(defaults.checksum);
    setVerifyAfter(defaults.verifyAfter);
    setPath(null);
    setPhase("idle");
    setPercent(null);
    setErrorMsg(null);
    opId.current = null;
  }, [open, defaults]);

  const running = phase === "running";

  async function choosePath() {
    try {
      const { save } = await import("@tauri-apps/plugin-dialog");
      const picked = await save({
        title: `Back up ${database}`,
        defaultPath: `${database}-${timestamp()}.bak`,
        filters: BAK_FILTER,
      });
      if (typeof picked === "string") setPath(picked);
    } catch (e) {
      toastError("Could not open the save dialog", asIpcError(e).message);
    }
  }

  async function runBackup() {
    if (!path) return;
    setPhase("running");
    setPercent(null);
    setErrorMsg(null);
    const channel = createBackupChannel((e) => {
      if (e.kind === "started") opId.current = e.operationId;
      else if (e.kind === "progress") setPercent(e.percent);
    });
    try {
      const summary = await databaseBackup(
        sessionId,
        database,
        path,
        { compression, checksum, verifyAfter },
        channel,
      );
      if (summary.cancelled) {
        toastInfo("Backup cancelled");
      } else {
        toastSuccess(`Backed up "${database}"`);
      }
      onClose();
    } catch (e) {
      const err = asIpcError(e);
      setPhase("error");
      setErrorMsg(err.message);
    }
  }

  function cancel() {
    if (opId.current) void backupCancel(opId.current);
  }

  const footer = running ? (
    <button type="button" className="ghost" onClick={cancel}>
      Cancel backup
    </button>
  ) : (
    <>
      <button type="button" className="ghost" onClick={onClose}>
        Close
      </button>
      <button
        type="button"
        className="primary"
        disabled={!path}
        onClick={() => void runBackup()}
      >
        {phase === "error" ? "Retry backup" : "Back up"}
      </button>
    </>
  );

  return (
    <Modal
      open={open}
      title={`Back up "${database}"`}
      onClose={running ? () => undefined : onClose}
      width={460}
      footer={footer}
    >
      <div className={styles.field}>
        <span className={styles.label}>Destination (.bak)</span>
        <div className={styles.pathRow}>
          <span
            className={`${styles.path} ${path ? "" : styles.pathEmpty}`}
            title={path ?? undefined}
          >
            {path ?? "No file chosen"}
          </span>
          <button
            type="button"
            onClick={() => void choosePath()}
            disabled={running}
          >
            Choose…
          </button>
        </div>
        <span className={styles.help}>
          This path is on the SQL Server host, not your machine. The server must
          be able to write to the chosen folder.
        </span>
      </div>

      <div className={styles.options}>
        <label className={styles.option}>
          <input
            type="checkbox"
            checked={compression}
            disabled={running}
            onChange={(e) => setCompression(e.target.checked)}
          />
          <span className={styles.optionText}>
            <span>Compression</span>
            <span className={styles.help}>
              Smaller, faster backups (editions that support it).
            </span>
          </span>
        </label>
        <label className={styles.option}>
          <input
            type="checkbox"
            checked={checksum}
            disabled={running}
            onChange={(e) => setChecksum(e.target.checked)}
          />
          <span className={styles.optionText}>
            <span>Checksum</span>
            <span className={styles.help}>Detect media/page corruption.</span>
          </span>
        </label>
        <label className={styles.option}>
          <input
            type="checkbox"
            checked={verifyAfter}
            disabled={running}
            onChange={(e) => setVerifyAfter(e.target.checked)}
          />
          <span className={styles.optionText}>
            <span>Verify after backup</span>
            <span className={styles.help}>
              Run RESTORE VERIFYONLY to confirm the file is readable.
            </span>
          </span>
        </label>
      </div>

      {running && <OperationProgress label="Backing up" percent={percent} />}
      {phase === "error" && errorMsg && (
        <div className={styles.errorBox}>{errorMsg}</div>
      )}
    </Modal>
  );
}
