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
import type { ConnectionSpec, DriverId } from "../ipc/types";
import { DRIVERS, driverDefaultPort, driverLabel } from "../lib/driverMeta";
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
  driver: DriverId;
  name: string;
  /** For SQLite this holds the database file path, not a hostname. */
  host: string;
  port: string;
  database: string;
  instance: string;
  /** MongoDB only: a full `mongodb://` / `mongodb+srv://` connection string. */
  uri: string;
  username: string;
  /** MongoDB only: the auth database for SCRAM (defaults to `admin` when blank). */
  authSource: string;
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
    driver: spec?.driver ?? "mssql",
    name: spec?.name ?? "",
    host: spec?.host ?? "",
    port: spec?.port != null ? String(spec.port) : "",
    database: spec?.database ?? "",
    instance: spec?.instance ?? "",
    uri: spec?.uri ?? "",
    // Both login variants carry a username (`sql_login` for SQL Server,
    // `scram_login` for MongoDB); `none` (SQLite / anonymous Mongo) has none.
    username:
      spec?.auth.method === "sql_login" || spec?.auth.method === "scram_login"
        ? spec.auth.username
        : "",
    authSource:
      spec?.auth.method === "scram_login" ? (spec.auth.auth_source ?? "") : "",
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
    const id = initial?.id ?? makeId();
    const name = form.name.trim() || form.host.trim() || "Untitled connection";

    // SQLite is fileless-auth: `host` is the database file path, no port/
    // instance/database/username, and TLS is irrelevant.
    if (form.driver === "sqlite") {
      return {
        id,
        name,
        driver: "sqlite",
        host: form.host.trim(),
        port: null,
        instance: null,
        uri: null,
        database: null,
        auth: { method: "none" },
        tls: { encrypt: true, trust_server_certificate: false },
        read_only: form.readOnly,
      };
    }

    // MongoDB accepts either a full `mongodb://` / `mongodb+srv://` URI (which
    // takes precedence) or discrete host/port/auth fields. Auth is anonymous
    // (`none`) when neither a username nor a URI is given, else SCRAM.
    if (form.driver === "mongodb") {
      const portNum = form.port.trim() ? Number(form.port.trim()) : null;
      const uri = form.uri.trim() || null;
      const username = form.username.trim();
      const anonymous = !username && !uri;
      return {
        id,
        name,
        driver: "mongodb",
        host: form.host.trim(),
        port: portNum != null && Number.isFinite(portNum) ? portNum : null,
        instance: null,
        uri,
        database: form.database.trim() || null,
        auth: anonymous
          ? { method: "none" }
          : {
              method: "scram_login",
              username,
              auth_source: form.authSource.trim() || null,
              mechanism: null,
            },
        tls: { encrypt: true, trust_server_certificate: form.trustCert },
        read_only: form.readOnly,
      };
    }

    const portNum = form.port.trim() ? Number(form.port.trim()) : null;
    return {
      id,
      name,
      driver: form.driver,
      host: form.host.trim(),
      port: portNum != null && Number.isFinite(portNum) ? portNum : null,
      // The named-instance field is MSSQL-only; never send it for pg/mysql.
      instance: form.driver === "mssql" ? form.instance.trim() || null : null,
      uri: null,
      database: form.database.trim() || null,
      auth: { method: "sql_login", username: form.username.trim() },
      tls: { encrypt: true, trust_server_certificate: form.trustCert },
      read_only: form.readOnly,
    };
  }

  function validate(): string | null {
    if (form.driver === "sqlite") {
      if (!form.host.trim()) return "Database file is required.";
      return null;
    }
    // MongoDB: either a URI or a host suffices; username/password are optional
    // (an anonymous connect is valid).
    if (form.driver === "mongodb") {
      if (!form.uri.trim() && !form.host.trim())
        return "A host or connection string is required.";
      return null;
    }
    if (!form.host.trim()) return "Host is required.";
    if (!form.username.trim()) return "Username is required.";
    return null;
  }

  async function browseSqliteFile() {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      multiple: false,
      filters: [
        { name: "SQLite database", extensions: ["db", "sqlite", "sqlite3"] },
      ],
    });
    if (typeof picked === "string") field("host", picked);
  }

  const isSqlite = form.driver === "sqlite";
  const isMssql = form.driver === "mssql";
  const isMongo = form.driver === "mongodb";

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
          <label htmlFor="conn-driver">Driver</label>
          <select
            id="conn-driver"
            value={form.driver}
            onChange={(e) => field("driver", e.target.value as DriverId)}
          >
            {DRIVERS.map((d) => (
              <option key={d} value={d}>
                {driverLabel(d)}
              </option>
            ))}
          </select>
        </div>

        <div className={styles.field}>
          <label htmlFor="conn-name">Name</label>
          <input
            id="conn-name"
            value={form.name}
            placeholder={`My ${driverLabel(form.driver)}`}
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            onChange={(e) => field("name", e.target.value)}
          />
        </div>

        {isSqlite ? (
          <div className={styles.field}>
            <label htmlFor="conn-file">Database file</label>
            <div className={styles.fileRow}>
              <input
                id="conn-file"
                className={styles.grow}
                value={form.host}
                placeholder="/path/to/database.sqlite"
                autoComplete="off"
                autoCorrect="off"
                autoCapitalize="off"
                spellCheck={false}
                onChange={(e) => field("host", e.target.value)}
              />
              <button type="button" onClick={() => void browseSqliteFile()}>
                Browse…
              </button>
            </div>
            <small className={styles.hint}>
              The file must already exist — Selene opens it, it won't create
              one.
            </small>
          </div>
        ) : (
          <>
            {/* MongoDB accepts a full connection string that overrides the
                discrete host/port/auth fields below (incl. `mongodb+srv://`). */}
            {isMongo && (
              <div className={styles.field}>
                <label htmlFor="conn-uri">Connection string (optional)</label>
                <input
                  id="conn-uri"
                  value={form.uri}
                  placeholder="mongodb+srv://cluster.example.net"
                  autoComplete="off"
                  autoCorrect="off"
                  autoCapitalize="off"
                  spellCheck={false}
                  onChange={(e) => field("uri", e.target.value)}
                />
                <small className={styles.hint}>
                  A URI (including <code>mongodb+srv://</code>) takes precedence
                  over the host/port fields below.
                </small>
              </div>
            )}

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
                  placeholder={String(driverDefaultPort(form.driver) ?? "")}
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
                  placeholder={
                    isMssql ? "master" : isMongo ? "admin" : "postgres"
                  }
                  autoComplete="off"
                  autoCorrect="off"
                  autoCapitalize="off"
                  spellCheck={false}
                  onChange={(e) => field("database", e.target.value)}
                />
              </div>
              {/* Named instance is an MSSQL-only concept. */}
              {isMssql && (
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
              )}
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

            {/* The SCRAM auth database is MongoDB-specific (defaults to `admin`). */}
            {isMongo && (
              <div className={styles.field}>
                <label htmlFor="conn-authsource">Auth source (optional)</label>
                <input
                  id="conn-authsource"
                  value={form.authSource}
                  placeholder="admin"
                  autoComplete="off"
                  autoCorrect="off"
                  autoCapitalize="off"
                  spellCheck={false}
                  onChange={(e) => field("authSource", e.target.value)}
                />
              </div>
            )}
          </>
        )}

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
          {/* TLS is irrelevant for a local SQLite file. */}
          {!isSqlite && (
            <label className={styles.toggle}>
              <input
                type="checkbox"
                checked={form.trustCert}
                onChange={(e) => field("trustCert", e.target.checked)}
              />
              <span>
                Trust server certificate
                <small>
                  Skip TLS validation (self-signed dev servers only).
                </small>
              </span>
            </label>
          )}
        </div>
        {/* Submit on Enter without a visible default button. */}
        <button type="submit" className="visually-hidden" tabIndex={-1}>
          Save and connect
        </button>
      </form>
    </Modal>
  );
}
