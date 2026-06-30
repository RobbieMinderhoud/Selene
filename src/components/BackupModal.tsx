/**
 * Back Up… dialog: `BACKUP DATABASE` to a `.bak` on the **SQL Server host**.
 *
 * `BACKUP` runs server-side, so the destination is a path on the *server's*
 * filesystem, not this machine — you browse the server's folders (or type a
 * path). For a local Docker server, a folder bind-mounted to your machine (e.g.
 * `/mnt/backups`) makes the resulting file appear locally. The destination is
 * pre-filled with the server's default backup directory.
 *
 * Options seed from the Database-backup settings. Progress (a real percentage
 * when the server reports it) streams over a `Channel<BackupEvent>`.
 */

import { useEffect, useRef, useState } from "react";

import {
  backupCancel,
  databaseBackup,
  serverDefaultBackupDir,
} from "../ipc/commands";
import { createBackupChannel } from "../ipc/channels";
import { asIpcError } from "../ipc/types";
import { baseName, dirName, joinServerPath } from "../lib/serverPath";
import { useSettingsStore } from "../state/settingsStore";
import { toastInfo, toastSuccess } from "../state/toastStore";
import { Modal } from "./Modal";
import { OperationProgress } from "./OperationProgress";
import { ServerPathBrowser } from "./ServerPathBrowser";
import styles from "./BackupRestore.module.css";

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
  const [path, setPath] = useState("");
  const [defaultDir, setDefaultDir] = useState("");
  const [browseOpen, setBrowseOpen] = useState(false);
  const [phase, setPhase] = useState<Phase>("idle");
  const [percent, setPercent] = useState<number | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const opId = useRef<string | null>(null);

  // Reset to current settings + a fresh suggested destination on open.
  useEffect(() => {
    if (!open) return;
    setCompression(defaults.compression);
    setChecksum(defaults.checksum);
    setVerifyAfter(defaults.verifyAfter);
    setPhase("idle");
    setPercent(null);
    setErrorMsg(null);
    opId.current = null;

    const fileName = `${database}-${timestamp()}.bak`;
    setPath(fileName);
    // Prefill the destination with the server's default backup directory.
    let cancelled = false;
    serverDefaultBackupDir(sessionId)
      .then((dir) => {
        if (cancelled) return;
        setDefaultDir(dir);
        if (dir) setPath(joinServerPath(dir, fileName));
      })
      .catch(() => {
        /* no default dir — the user types or browses a path */
      });
    return () => {
      cancelled = true;
    };
  }, [open, defaults, sessionId, database]);

  const running = phase === "running";

  async function runBackup() {
    const dest = path.trim();
    if (!dest) return;
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
        dest,
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
      setPhase("error");
      setErrorMsg(asIpcError(e).message);
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
        disabled={!path.trim()}
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
        <span className={styles.label}>Destination (.bak) on the server</span>
        <div className={styles.pathRow}>
          <input
            className={styles.confirmInput}
            value={path}
            spellCheck={false}
            autoComplete="off"
            aria-label="Backup destination path"
            disabled={running}
            onChange={(e) => setPath(e.target.value)}
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
          This path is on the SQL Server host, not your machine. For a local
          Docker server, choose a folder mounted to your machine (e.g.
          /mnt/backups) so the file appears locally.
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

      <ServerPathBrowser
        open={browseOpen}
        sessionId={sessionId}
        mode="saveBak"
        initialPath={dirName(path) || defaultDir}
        defaultFileName={baseName(path) || `${database}-${timestamp()}.bak`}
        onClose={() => setBrowseOpen(false)}
        onPick={(picked) => {
          setPath(picked);
          setBrowseOpen(false);
        }}
      />
    </Modal>
  );
}
