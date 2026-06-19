/**
 * Create/edit a connection. Test / Save / Connect.
 *
 * SECURITY: the password lives ONLY in this component's local state. It is
 * passed to `connectionSave` / `connectionTest` and never written to any store,
 * never logged. Editing an existing connection leaves the password blank; an
 * empty password on save means "leave the stored secret untouched" (matching the
 * backend's `connection_save` semantics).
 */

import { useState } from "react";

import { connectionSave, connectionTest } from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import type { ConnectionSpec } from "../ipc/types";
import { useSettingsStore } from "../state/settingsStore";
import { toastError, toastSuccess } from "../state/toastStore";
import { Modal } from "./Modal";
import styles from "./ConnectionDialog.module.css";

interface ConnectionDialogProps {
  open: boolean;
  /** Existing spec to edit, or `null` for a new connection. */
  initial: ConnectionSpec | null;
  onClose: () => void;
  /** Called with the saved spec after a successful Save. */
  onSaved: (spec: ConnectionSpec) => void;
  /** Called with the saved spec after Save when "Connect" was pressed. */
  onSaveAndConnect: (spec: ConnectionSpec) => void;
}

interface FormState {
  name: string;
  host: string;
  port: string;
  database: string;
  instance: string;
  username: string;
  readOnly: boolean;
  trustCert: boolean;
}

function specToForm(spec: ConnectionSpec | null): FormState {
  // For new connections, seed the read-only flag from the user's default setting.
  // For existing connections, always use the stored value (ignore the setting).
  const defaultReadOnly =
    spec === null
      ? useSettingsStore.getState().query.defaultConnectionReadOnly
      : false;
  return {
    name: spec?.name ?? "",
    host: spec?.host ?? "",
    port: spec?.port != null ? String(spec.port) : "",
    database: spec?.database ?? "",
    instance: spec?.instance ?? "",
    username: spec?.auth.username ?? "",
    readOnly: spec?.read_only ?? defaultReadOnly,
    trustCert: spec?.tls.trust_server_certificate ?? false,
  };
}

function makeId(): string {
  // crypto.randomUUID is available in the Tauri webview.
  return crypto.randomUUID();
}

export function ConnectionDialog({
  open,
  initial,
  onClose,
  onSaved,
  onSaveAndConnect,
}: ConnectionDialogProps) {
  const [form, setForm] = useState<FormState>(() => specToForm(initial));
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState<null | "test" | "save" | "connect">(null);

  // Re-seed the form whenever the dialog opens (for any spec, including new connections).
  // `undefined` is used as a sentinel meaning "not yet seeded for this open session";
  // it always differs from `null` and from any real id, so the condition always fires
  // on the first render after the dialog opens.
  const [seededFor, setSeededFor] = useState<string | null | undefined>(
    initial?.id ?? null,
  );
  if (!open && seededFor !== undefined) {
    setSeededFor(undefined);
  }
  if (open && seededFor !== (initial?.id ?? null)) {
    setForm(specToForm(initial));
    setPassword("");
    setSeededFor(initial?.id ?? null);
  }

  function field<K extends keyof FormState>(key: K, value: FormState[K]) {
    setForm((f) => ({ ...f, [key]: value }));
  }

  function buildSpec(): ConnectionSpec {
    const portNum = form.port.trim() ? Number(form.port.trim()) : null;
    return {
      id: initial?.id ?? makeId(),
      name: form.name.trim() || form.host.trim() || "Untitled connection",
      driver: "mssql",
      host: form.host.trim(),
      port: portNum != null && Number.isFinite(portNum) ? portNum : null,
      instance: form.instance.trim() || null,
      database: form.database.trim() || null,
      auth: { method: "sql_login", username: form.username.trim() },
      tls: { encrypt: true, trust_server_certificate: form.trustCert },
      read_only: form.readOnly,
    };
  }

  function validate(): string | null {
    if (!form.host.trim()) return "Host is required.";
    if (!form.username.trim()) return "Username is required.";
    return null;
  }

  async function handleTest() {
    const err = validate();
    if (err) {
      toastError(err);
      return;
    }
    setBusy("test");
    try {
      const report = await connectionTest(buildSpec(), password || undefined);
      const version = report.server_version ?? "unknown version";
      toastSuccess(`Connected in ${report.elapsed_ms} ms · ${version}`);
    } catch (e) {
      const ipc = asIpcError(e);
      toastError("Connection test failed", ipc.message);
    } finally {
      setBusy(null);
    }
  }

  async function handleSave(thenConnect: boolean) {
    const err = validate();
    if (err) {
      toastError(err);
      return;
    }
    setBusy(thenConnect ? "connect" : "save");
    try {
      const saved = await connectionSave(buildSpec(), password || undefined);
      // Clear the password from local state immediately after use.
      setPassword("");
      if (thenConnect) onSaveAndConnect(saved);
      else {
        toastSuccess("Connection saved.");
        onSaved(saved);
      }
    } catch (e) {
      const ipc = asIpcError(e);
      toastError("Could not save connection", ipc.message);
    } finally {
      setBusy(null);
    }
  }

  const footer = (
    <>
      <button type="button" onClick={onClose} disabled={busy !== null}>
        Cancel
      </button>
      <button type="button" onClick={handleTest} disabled={busy !== null}>
        {busy === "test" ? "Testing…" : "Test"}
      </button>
      <button
        type="button"
        onClick={() => handleSave(false)}
        disabled={busy !== null}
      >
        {busy === "save" ? "Saving…" : "Save"}
      </button>
      <button
        type="button"
        className="primary"
        onClick={() => handleSave(true)}
        disabled={busy !== null}
      >
        {busy === "connect" ? "Connecting…" : "Save & Connect"}
      </button>
    </>
  );

  return (
    <Modal
      open={open}
      title={initial ? "Edit connection" : "New connection"}
      onClose={onClose}
      footer={footer}
      width={520}
    >
      <form
        className={styles.form}
        onSubmit={(e) => {
          e.preventDefault();
          handleSave(true);
        }}
      >
        <div className={styles.field}>
          <label htmlFor="conn-name">Name</label>
          <input
            id="conn-name"
            value={form.name}
            placeholder="My SQL Server"
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            onChange={(e) => field("name", e.target.value)}
          />
        </div>

        <div className={styles.row}>
          <div className={`${styles.field} ${styles.grow}`}>
            <label htmlFor="conn-host">Host</label>
            <input
              id="conn-host"
              value={form.host}
              placeholder="localhost"
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              onChange={(e) => field("host", e.target.value)}
            />
          </div>
          <div className={styles.fieldNarrow}>
            <label htmlFor="conn-port">Port</label>
            <input
              id="conn-port"
              value={form.port}
              placeholder="1433"
              inputMode="numeric"
              autoComplete="off"
              autoCorrect="off"
              spellCheck={false}
              onChange={(e) => field("port", e.target.value)}
            />
          </div>
        </div>

        <div className={styles.row}>
          <div className={`${styles.field} ${styles.grow}`}>
            <label htmlFor="conn-db">Database (optional)</label>
            <input
              id="conn-db"
              value={form.database}
              placeholder="master"
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              onChange={(e) => field("database", e.target.value)}
            />
          </div>
          <div className={`${styles.field} ${styles.grow}`}>
            <label htmlFor="conn-instance">Instance (optional)</label>
            <input
              id="conn-instance"
              value={form.instance}
              placeholder="SQLEXPRESS"
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              onChange={(e) => field("instance", e.target.value)}
            />
          </div>
        </div>

        <div className={styles.row}>
          <div className={`${styles.field} ${styles.grow}`}>
            <label htmlFor="conn-user">Username</label>
            <input
              id="conn-user"
              value={form.username}
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              onChange={(e) => field("username", e.target.value)}
            />
          </div>
          <div className={`${styles.field} ${styles.grow}`}>
            <label htmlFor="conn-pass">Password</label>
            <input
              id="conn-pass"
              type="password"
              value={password}
              autoComplete="new-password"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              placeholder={initial ? "•••••• (unchanged)" : ""}
              onChange={(e) => setPassword(e.target.value)}
            />
          </div>
        </div>

        <div className={styles.toggles}>
          <label className={styles.toggle}>
            <input
              type="checkbox"
              checked={form.readOnly}
              onChange={(e) => field("readOnly", e.target.checked)}
            />
            <span>
              Read-only
              <small>Block any non-SELECT statement for this connection.</small>
            </span>
          </label>
          <label className={styles.toggle}>
            <input
              type="checkbox"
              checked={form.trustCert}
              onChange={(e) => field("trustCert", e.target.checked)}
            />
            <span>
              Trust server certificate
              <small>Skip TLS validation (self-signed dev servers only).</small>
            </span>
          </label>
        </div>
        {/* Submit on Enter without a visible default button. */}
        <button type="submit" className="visually-hidden" tabIndex={-1}>
          Save and connect
        </button>
      </form>
    </Modal>
  );
}
