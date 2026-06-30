import { useState } from "react";
import { Modal } from "./Modal";
import { useThemeStore, type ThemeMode } from "../state/themeStore";
import {
  useSettingsStore,
  type CopyFormat,
  type CsvDelimiter,
  type CsvLineEnding,
  type CsvQuoteChar,
  type CsvQuoteStyle,
  type ImportQuoteChar,
  type NullDisplay,
  type ReduceMotion,
  type RowDensity,
} from "../state/settingsStore";
import {
  connectionsList,
  connectionsImport,
  fileRead,
  fileWrite,
  setHealthCheck,
} from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import { toastError, toastSuccess } from "../state/toastStore";
import { CheckIcon } from "./icons";
import styles from "./SettingsModal.module.css";

interface SettingsModalProps {
  open: boolean;
  onClose: () => void;
  /** Called after a successful backup import that included connections. */
  onConnectionsChanged?: () => void;
}

type Tab = "general" | "csv" | "multiTarget" | "backup";

const TAB_LABELS: Record<Tab, string> = {
  general: "General",
  csv: "CSV",
  multiTarget: "Multi-target",
  backup: "Backup & restore",
};

interface BackupFile {
  version: number;
  connections: unknown[];
  settings: unknown;
}

const THEMES: {
  mode: ThemeMode;
  label: string;
  swatches: [string, string, string];
}[] = [
  { mode: "dark", label: "Dark", swatches: ["#0e1116", "#4493f8", "#e6edf3"] },
  {
    mode: "light",
    label: "Light",
    swatches: ["#f6f8fa", "#0969da", "#1f2328"],
  },
  {
    mode: "retro",
    label: "Retro",
    swatches: ["#f2e5bc", "#458588", "#3c3836"],
  },
];

/** Checkbox toggle row with label and optional helper text. */
function SettingToggle({
  label,
  help,
  value,
  onChange,
}: {
  label: string;
  help?: string;
  value: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className={styles.row}>
      <span className={styles.rowLabel}>
        {label}
        {help && <small className={styles.rowHelp}>{help}</small>}
      </span>
      <button
        type="button"
        role="switch"
        aria-checked={value}
        className={styles.toggle}
        data-on={value ? "true" : "false"}
        onClick={() => onChange(!value)}
        aria-label={label}
      />
    </div>
  );
}

/** Select row with label and typed options. */
function SettingSelect<T extends string | number>({
  label,
  help,
  value,
  options,
  onChange,
}: {
  label: string;
  help?: string;
  value: T;
  options: [T, string][];
  onChange: (v: T) => void;
}) {
  return (
    <div className={styles.row}>
      <span className={styles.rowLabel}>
        {label}
        {help && <small className={styles.rowHelp}>{help}</small>}
      </span>
      <select
        className={styles.select}
        value={String(value)}
        onChange={(e) => {
          const raw = e.target.value;
          const found = options.find(([k]) => String(k) === raw);
          if (found) onChange(found[0]);
        }}
      >
        {options.map(([k, label]) => (
          <option key={String(k)} value={String(k)}>
            {label}
          </option>
        ))}
      </select>
    </div>
  );
}

/** Full-width labelled multi-line text field (for the SQL filter default). */
function SettingTextarea({
  label,
  help,
  value,
  onChange,
}: {
  label: string;
  help?: string;
  value: string;
  onChange: (v: string) => void;
}) {
  return (
    <div className={styles.fieldCol}>
      <span className={styles.rowLabel}>
        {label}
        {help && <small className={styles.rowHelp}>{help}</small>}
      </span>
      <textarea
        className={styles.textarea}
        rows={4}
        spellCheck={false}
        value={value}
        onChange={(e) => onChange(e.target.value)}
      />
    </div>
  );
}

const BACKUP_FILTER = [{ name: "Selene Backup", extensions: ["json"] }];

async function exportBackup(): Promise<void> {
  try {
    const { save } = await import("@tauri-apps/plugin-dialog");
    const path = await save({
      title: "Export backup",
      defaultPath: "selene-backup.json",
      filters: BACKUP_FILTER,
    });
    if (!path) return;
    const connections = await connectionsList();
    const settings = (() => {
      const s = useSettingsStore.getState();
      return {
        appearance: s.appearance,
        editor: s.editor,
        results: s.results,
        query: s.query,
        connection: s.connection,
        export: s.export,
        import: s.import,
        backup: s.backup,
        multiTarget: s.multiTarget,
      };
    })();
    const backup: BackupFile = { version: 1, connections, settings };
    await fileWrite(path, JSON.stringify(backup, null, 2));
    toastSuccess("Backup exported");
  } catch (e) {
    toastError("Export failed", asIpcError(e).message);
  }
}

async function importBackup(onConnectionsChanged: () => void): Promise<void> {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const path = await open({
      title: "Import backup",
      multiple: false,
      filters: BACKUP_FILTER,
    });
    if (typeof path !== "string") return;
    const raw = await fileRead(path);
    const backup = JSON.parse(raw) as Partial<BackupFile>;
    if (typeof backup !== "object" || backup === null || backup.version !== 1) {
      toastError(
        "Import failed",
        "Not a valid Selene backup file (expected version 1).",
      );
      return;
    }
    let connCount = 0;
    if (Array.isArray(backup.connections) && backup.connections.length > 0) {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      await connectionsImport(backup.connections as any);
      connCount = backup.connections.length;
      onConnectionsChanged();
    }
    if (backup.settings != null) {
      useSettingsStore.getState().importSettings(backup.settings);
    }
    const parts: string[] = [];
    if (connCount > 0)
      parts.push(`${connCount} connection${connCount === 1 ? "" : "s"}`);
    if (backup.settings != null) parts.push("settings");
    toastSuccess(`Imported ${parts.join(" and ")}`);
  } catch (e) {
    toastError("Import failed", asIpcError(e).message);
  }
}

const ROW_LIMIT_OPTIONS: [number, string][] = [
  [1_000, "1,000"],
  [10_000, "10,000"],
  [50_000, "50,000"],
  [100_000, "100,000"],
];

export function SettingsModal({
  open,
  onClose,
  onConnectionsChanged,
}: SettingsModalProps) {
  const [activeTab, setActiveTab] = useState<Tab>("general");

  const mode = useThemeStore((s) => s.mode);
  const setTheme = useThemeStore((s) => s.setTheme);

  const s = useSettingsStore();
  const setSection = useSettingsStore((st) => st.set);
  const resetSettings = useSettingsStore((st) => st.resetSettings);

  return (
    <Modal open={open} title="Settings" onClose={onClose} width={520}>
      {/* ── Tab navigation ────────────────────────────────────────────── */}
      <div className={styles.tabList} role="tablist">
        {(["general", "csv", "multiTarget", "backup"] as Tab[]).map((tab) => (
          <button
            key={tab}
            type="button"
            role="tab"
            aria-selected={activeTab === tab}
            className={`${styles.tab} ${activeTab === tab ? styles.tabActive : ""}`}
            onClick={() => setActiveTab(tab)}
          >
            {TAB_LABELS[tab]}
          </button>
        ))}
      </div>

      {/* ── General tab ───────────────────────────────────────────────── */}
      {activeTab === "general" && (
        <div key="general" className={styles.tabPanel} role="tabpanel">
          {/* Appearance */}
          <section className={styles.section}>
            <h3 className={styles.sectionLabel}>Appearance</h3>
            <div className={styles.themeOptions}>
              {THEMES.map(({ mode: m, label, swatches }) => (
                <button
                  key={m}
                  type="button"
                  className={`${styles.themeOption} ${mode === m ? styles.active : ""}`}
                  onClick={() => setTheme(m)}
                  aria-pressed={mode === m}
                >
                  <span className={styles.swatch}>
                    {swatches.map((color, i) => (
                      <span
                        key={i}
                        className={styles.swatchSegment}
                        style={{ background: color }}
                      />
                    ))}
                  </span>
                  <span className={styles.themeLabel}>{label}</span>
                  {mode === m && (
                    <span className={styles.check} aria-hidden>
                      <CheckIcon />
                    </span>
                  )}
                </button>
              ))}
            </div>
            <SettingSelect<ReduceMotion>
              label="Reduce motion"
              help='Choose "On" to disable app animations. Your OS reduced-motion preference still applies regardless.'
              value={s.appearance.reduceMotion}
              options={[
                ["system", "Follow system"],
                ["on", "On"],
                ["off", "Off"],
              ]}
              onChange={(v) => setSection("appearance", { reduceMotion: v })}
            />
          </section>

          {/* Editor */}
          <section className={styles.section}>
            <h3 className={styles.sectionLabel}>Editor</h3>
            <SettingSelect<number>
              label="Font size"
              value={s.editor.fontSize}
              options={[
                [11, "11 px"],
                [12, "12 px"],
                [13, "13 px"],
                [14, "14 px"],
                [16, "16 px"],
                [18, "18 px"],
              ]}
              onChange={(v) => setSection("editor", { fontSize: v as 13 })}
            />
            <SettingSelect<2 | 4 | 8>
              label="Tab size"
              value={s.editor.tabSize}
              options={[
                [2, "2 spaces"],
                [4, "4 spaces"],
                [8, "8 spaces"],
              ]}
              onChange={(v) => setSection("editor", { tabSize: v })}
            />
            <SettingToggle
              label="Word wrap"
              value={s.editor.wordWrap}
              onChange={(v) => setSection("editor", { wordWrap: v })}
            />
            <SettingToggle
              label="Line numbers"
              value={s.editor.lineNumbers}
              onChange={(v) => setSection("editor", { lineNumbers: v })}
            />
            <SettingToggle
              label="Autocompletion"
              value={s.editor.autocompletion}
              onChange={(v) => setSection("editor", { autocompletion: v })}
            />
            <SettingToggle
              label="Schema autocomplete"
              help="Suggest tables and columns from the connected database. Columns load on demand when a table is referenced."
              value={s.editor.schemaCompletion}
              onChange={(v) => setSection("editor", { schemaCompletion: v })}
            />
            <SettingToggle
              label="Bracket matching & auto-close"
              value={s.editor.bracketPairs}
              onChange={(v) => setSection("editor", { bracketPairs: v })}
            />
            <SettingToggle
              label="Uppercase SQL keywords"
              help="Completions write SELECT, FROM, WHERE in upper-case."
              value={s.editor.upperCaseKeywords}
              onChange={(v) => setSection("editor", { upperCaseKeywords: v })}
            />
          </section>

          {/* Results */}
          <section className={styles.section}>
            <h3 className={styles.sectionLabel}>Results</h3>
            <SettingSelect<number>
              label="Default row limit"
              help="Hard cap on rows returned per query. Lower values keep the grid fast."
              value={s.results.defaultRowLimit ?? 50_000}
              options={ROW_LIMIT_OPTIONS}
              onChange={(v) => setSection("results", { defaultRowLimit: v })}
            />
            <SettingSelect<RowDensity>
              label="Row density"
              value={s.results.density}
              options={[
                ["compact", "Compact"],
                ["normal", "Normal"],
                ["comfortable", "Comfortable"],
              ]}
              onChange={(v) => setSection("results", { density: v })}
            />
            <SettingSelect<NullDisplay>
              label="Null display"
              help="How NULL cell values appear in the grid."
              value={s.results.nullDisplay}
              options={[
                ["NULL", "NULL"],
                ["(null)", "(null)"],
                ["·", "· (middle dot)"],
                ["", "(blank)"],
              ]}
              onChange={(v) => setSection("results", { nullDisplay: v })}
            />
            <SettingSelect<CopyFormat>
              label="Copy format"
              help="Used when copying cells with Cmd/Ctrl+C. Right-click the grid to copy as a different format once."
              value={s.results.copyFormat}
              options={[
                ["tab", "Tab-separated (Excel / Sheets)"],
                ["comma", "Comma-separated (CSV)"],
                ["markdown", "Markdown table"],
                ["html", "HTML table"],
              ]}
              onChange={(v) => setSection("results", { copyFormat: v })}
            />
            <SettingToggle
              label="Include headers when copying"
              help="Prepend the column-name row. (Markdown always includes it.)"
              value={s.results.copyIncludeHeaders}
              onChange={(v) => setSection("results", { copyIncludeHeaders: v })}
            />
          </section>

          {/* Query behaviour */}
          <section className={styles.section}>
            <h3 className={styles.sectionLabel}>Query behaviour</h3>
            <SettingToggle
              label="Confirm before running on read-write connection"
              help="Shows a confirmation dialog before any query on a non-read-only connection."
              value={s.query.confirmOnReadWrite}
              onChange={(v) => setSection("query", { confirmOnReadWrite: v })}
            />
            <SettingToggle
              label="Default new connections to read-only"
              help="Pre-checks the read-only option when creating a new connection."
              value={s.query.defaultConnectionReadOnly}
              onChange={(v) =>
                setSection("query", { defaultConnectionReadOnly: v })
              }
            />
            <SettingToggle
              label="Auto-connect opened files by name"
              help="When you open a SQL file, connect it to the saved connection whose name appears in the file name (e.g. pr02db02b_shared_01.sql → pr02db02b)."
              value={s.query.autoConnectFromFile}
              onChange={(v) => setSection("query", { autoConnectFromFile: v })}
            />
          </section>

          {/* Connection health */}
          <section className={styles.section}>
            <h3 className={styles.sectionLabel}>Connection health</h3>
            <SettingToggle
              label="Auto-close dropped connections"
              help="Periodically check that each open connection is still reachable and automatically close any that stopped responding (e.g. after Wi-Fi/VPN loss), instead of leaving the app waiting."
              value={s.connection.healthCheck}
              onChange={(v) => {
                setSection("connection", { healthCheck: v });
                void setHealthCheck(v, s.connection.healthCheckIntervalSecs);
              }}
            />
            <SettingSelect<number>
              label="Check interval"
              help="How often to ping open connections."
              value={s.connection.healthCheckIntervalSecs}
              options={[
                [3, "Every 3 seconds"],
                [5, "Every 5 seconds"],
                [10, "Every 10 seconds"],
                [30, "Every 30 seconds"],
              ]}
              onChange={(v) => {
                setSection("connection", { healthCheckIntervalSecs: v });
                void setHealthCheck(s.connection.healthCheck, v);
              }}
            />
          </section>

          {/* Database backup (BACKUP DATABASE defaults) */}
          <section className={styles.section}>
            <h3 className={styles.sectionLabel}>Database backup</h3>
            <SettingToggle
              label="Compression"
              help="Default WITH COMPRESSION when backing up a database. Smaller, faster .bak files on editions that support it."
              value={s.backup.compression}
              onChange={(v) => setSection("backup", { compression: v })}
            />
            <SettingToggle
              label="Checksum"
              help="Default WITH CHECKSUM to detect media/page corruption during backup and restore."
              value={s.backup.checksum}
              onChange={(v) => setSection("backup", { checksum: v })}
            />
            <SettingToggle
              label="Verify after backup"
              help="Run RESTORE VERIFYONLY once the backup is written to confirm the file is readable."
              value={s.backup.verifyAfter}
              onChange={(v) => setSection("backup", { verifyAfter: v })}
            />
          </section>
        </div>
      )}

      {/* ── CSV tab (export + import) ─────────────────────────────────── */}
      {activeTab === "csv" && (
        <div key="csv" className={styles.tabPanel} role="tabpanel">
          <section className={styles.section}>
            <h3 className={styles.sectionLabel}>Export</h3>
            <SettingSelect<CsvDelimiter>
              label="Delimiter"
              help="Field separator written between columns."
              value={s.export.delimiter}
              options={[
                [";", "; (semicolon)"],
                [",", ", (comma)"],
                ["\t", "Tab"],
                ["|", "| (pipe)"],
              ]}
              onChange={(v) => setSection("export", { delimiter: v })}
            />
            <SettingSelect<CsvQuoteChar>
              label="Quote character"
              help="Character used to wrap fields that contain the delimiter or newlines."
              value={s.export.quoteChar}
              options={[
                ['"', '" (double quote)'],
                ["'", "' (single quote)"],
              ]}
              onChange={(v) => setSection("export", { quoteChar: v })}
            />
            <SettingSelect<CsvQuoteStyle>
              label="Quoting"
              help="When to apply the quote character around fields."
              value={s.export.quoteStyle}
              options={[
                ["necessary", "When necessary (RFC-4180)"],
                ["always", "Always"],
                ["non_numeric", "Non-numeric fields"],
                ["never", "Never"],
              ]}
              onChange={(v) => setSection("export", { quoteStyle: v })}
            />
            <SettingSelect<CsvLineEnding>
              label="Line ending"
              help="Record terminator. CRLF is required by RFC-4180 and expected by Excel."
              value={s.export.lineEnding}
              options={[
                ["crlf", "CRLF (Windows / RFC-4180)"],
                ["lf", "LF (Unix)"],
              ]}
              onChange={(v) => setSection("export", { lineEnding: v })}
            />
            <SettingToggle
              label="Include header row"
              help="Write column names as the first row of the file."
              value={s.export.includeHeader}
              onChange={(v) => setSection("export", { includeHeader: v })}
            />
            <SettingToggle
              label="UTF-8 BOM"
              help="Prepend a byte-order mark so Excel opens the file without a re-encoding prompt on Windows."
              value={s.export.bom}
              onChange={(v) => setSection("export", { bom: v })}
            />
          </section>
          <section className={styles.section}>
            <h3 className={styles.sectionLabel}>Import</h3>
            <SettingSelect<CsvDelimiter>
              label="Delimiter"
              help="Default field separator. You can override it per import in the mapping menu."
              value={s.import.delimiter}
              options={[
                [",", ", (comma)"],
                [";", "; (semicolon)"],
                ["\t", "Tab"],
                ["|", "| (pipe)"],
              ]}
              onChange={(v) => setSection("import", { delimiter: v })}
            />
            <SettingSelect<ImportQuoteChar>
              label="Quote character"
              help="Character used to wrap fields containing the delimiter or newlines. Choose None for files that never quote."
              value={s.import.quoteChar}
              options={[
                ['"', '" (double quote)'],
                ["'", "' (single quote)"],
                ["none", "None (no quoting)"],
              ]}
              onChange={(v) => setSection("import", { quoteChar: v })}
            />
            <SettingToggle
              label="First row is a header"
              help="Treat the first CSV row as column names rather than data."
              value={s.import.hasHeader}
              onChange={(v) => setSection("import", { hasHeader: v })}
            />
            <SettingToggle
              label="Treat empty fields as NULL"
              help="An empty cell becomes SQL NULL; otherwise an empty string for text columns."
              value={s.import.emptyAsNull}
              onChange={(v) => setSection("import", { emptyAsNull: v })}
            />
            <SettingToggle
              label="Abort import on first bad row"
              help="Roll back the whole import if any value cannot be converted. Turn off to skip bad rows and report how many were skipped."
              value={s.import.atomic}
              onChange={(v) => setSection("import", { atomic: v })}
            />
          </section>
        </div>
      )}

      {/* ── Multi-target tab ──────────────────────────────────────────── */}
      {activeTab === "multiTarget" && (
        <div key="multiTarget" className={styles.tabPanel} role="tabpanel">
          <section className={styles.section}>
            <h3 className={styles.sectionLabel}>Run on multiple targets</h3>
            <SettingTextarea
              label="Default database filter"
              help="Pre-fills the database-filter editor for a new multi-target view. Should return one column of database names (e.g. from sys.databases)."
              value={s.multiTarget.defaultFilterQuery}
              onChange={(v) =>
                setSection("multiTarget", { defaultFilterQuery: v })
              }
            />
            <SettingSelect<number>
              label="Max parallel servers"
              help="How many servers run concurrently. Databases within a server always run one at a time."
              value={s.multiTarget.maxParallelServers}
              options={[
                [1, "1 (sequential)"],
                [2, "2"],
                [4, "4"],
                [8, "8"],
              ]}
              onChange={(v) =>
                setSection("multiTarget", { maxParallelServers: v })
              }
            />
            <SettingSelect<number>
              label="Pause after failures reach"
              help="When this share of the run's targets has failed, the run pauses and asks whether to continue or stop. It prompts at most once per run. Off disables it."
              value={s.multiTarget.pauseFailurePercent}
              options={[
                [0, "Off"],
                [5, "5%"],
                [10, "10%"],
                [20, "20%"],
                [25, "25%"],
                [50, "50%"],
              ]}
              onChange={(v) =>
                setSection("multiTarget", { pauseFailurePercent: v })
              }
            />
          </section>
        </div>
      )}

      {/* ── Backup tab ────────────────────────────────────────────────── */}
      {activeTab === "backup" && (
        <div key="backup" className={styles.tabPanel} role="tabpanel">
          <section className={styles.section}>
            <h3 className={styles.sectionLabel}>Connections &amp; settings</h3>
            <div className={styles.backupRow}>
              <div className={styles.backupRowText}>
                <span className={styles.backupRowLabel}>Export backup</span>
                <span className={styles.backupRowHelp}>
                  Saves connections and settings to a JSON file. Passwords are
                  stored in the system keychain and are not included.
                </span>
              </div>
              <button type="button" onClick={() => void exportBackup()}>
                Export…
              </button>
            </div>
            <div className={styles.backupRow}>
              <div className={styles.backupRowText}>
                <span className={styles.backupRowLabel}>Import backup</span>
                <span className={styles.backupRowHelp}>
                  Restores from a backup file. Connections with matching IDs are
                  updated; new ones are added. You will need to re-enter
                  passwords after importing.
                </span>
              </div>
              <button
                type="button"
                onClick={() =>
                  void importBackup(onConnectionsChanged ?? (() => undefined))
                }
              >
                Import…
              </button>
            </div>
          </section>
        </div>
      )}

      <footer className={styles.footer}>
        <button type="button" className="ghost" onClick={resetSettings}>
          Reset to defaults
        </button>
      </footer>
    </Modal>
  );
}
