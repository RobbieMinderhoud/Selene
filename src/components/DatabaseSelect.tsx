/**
 * Toolbar dropdown for the currently active database on a session.
 *
 * Fetches the database list once (cached by TanStack Query) and renders a
 * `<select>` whose value tracks the per-tab `currentDatabase`. The selection
 * is updated optimistically; if `onSelect` throws it reverts.
 */

import { useEffect, useState } from "react";

import { useDatabases } from "../lib/queries";
import styles from "./DatabaseSelect.module.css";

interface DatabaseSelectProps {
  sessionId: string;
  currentDatabase: string | null;
  /**
   * Called when the user picks a different database. Should run `USE [db]` and
   * update the store on success. Must throw on failure so the selection reverts.
   */
  onSelect: (db: string) => Promise<void>;
}

export function DatabaseSelect({
  sessionId,
  currentDatabase,
  onSelect,
}: DatabaseSelectProps) {
  const { data: databases } = useDatabases(sessionId, true);
  const [selected, setSelected] = useState(currentDatabase ?? "");

  // Sync local selection when the db changes from outside (e.g. after a USE in a query).
  useEffect(() => {
    setSelected(currentDatabase ?? "");
  }, [currentDatabase]);

  const allDbs = databases ?? [];
  const currentIsInList =
    currentDatabase != null && allDbs.some((d) => d.name === currentDatabase);

  async function handleChange(e: React.ChangeEvent<HTMLSelectElement>) {
    const db = e.target.value;
    if (!db || db === currentDatabase) return;
    const prev = selected;
    setSelected(db); // optimistic
    try {
      await onSelect(db);
    } catch {
      setSelected(prev); // revert on failure
    }
  }

  return (
    <select
      className={styles.select}
      value={selected}
      onChange={handleChange}
      aria-label="Switch database"
      title="Switch database"
    >
      {/* Placeholder when no database is known yet */}
      {!currentDatabase && !allDbs.length && (
        <option value="" disabled>
          Database
        </option>
      )}
      {/* Keep the current database selectable even before the list loads */}
      {currentDatabase && !currentIsInList && (
        <option value={currentDatabase}>{currentDatabase}</option>
      )}
      {allDbs.map((db) => (
        <option key={db.name} value={db.name}>
          {db.name}
        </option>
      ))}
    </select>
  );
}
