/**
 * Lazy file browser for one workspace folder. Mirrors {@link SchemaTree}: each
 * directory fetches its children on first expand via TanStack Query
 * (`useDir`), so a large project tree loads incrementally. Clicking a `.sql`
 * file opens it as a tab; an already-open file reads as active.
 *
 * The file-sync reconciler invalidates a directory's `dir` query when its
 * contents change on disk (file added/removed), so the tree self-refreshes.
 */

import { useState } from "react";

import type { FsEntry } from "../ipc/types";
import { closeFolder, openFileFromPath } from "../lib/fileActions";
import { matches } from "../lib/filterMatch";
import { basename } from "../lib/path";
import { useDir } from "../lib/queries";
import { useEditorStore } from "../state/editorStore";
import { CaretIcon, CloseIcon, FileIcon, FolderIcon } from "./icons";
import styles from "./FileTree.module.css";

function FileRow({
  entry,
  depth,
  filter,
}: {
  entry: FsEntry;
  depth: number;
  filter: string;
}) {
  // Narrow subscription: only whether *this* path is open, so other tabs'
  // edits don't repaint the whole tree.
  const isOpen = useEditorStore((s) =>
    s.tabs.some((t) => t.filePath === entry.path),
  );
  if (!matches(entry.name, filter)) return null;
  return (
    <div
      className={`${styles.row} ${isOpen ? styles.active : ""}`}
      style={{ paddingLeft: depth * 14 + 6 }}
      role="treeitem"
      tabIndex={0}
      title={entry.path}
      onClick={() => void openFileFromPath(entry.path)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          void openFileFromPath(entry.path);
        }
      }}
    >
      <span className={styles.caretSpacer} aria-hidden />
      <span className={styles.icon} aria-hidden>
        <FileIcon />
      </span>
      <span className={styles.label}>{entry.name}</span>
    </div>
  );
}

function DirNode({
  entry,
  depth,
  filter,
}: {
  entry: FsEntry;
  depth: number;
  filter: string;
}) {
  const [open, setOpen] = useState(false);
  // Self-hide unless the name matches or the folder is expanded (so drilling in
  // is never undone by typing). Shallow: collapsed folders aren't fetched.
  if (!matches(entry.name, filter) && !open) return null;
  return (
    <>
      <div
        className={styles.row}
        style={{ paddingLeft: depth * 14 + 6 }}
        role="treeitem"
        aria-expanded={open}
        tabIndex={0}
        title={entry.path}
        onClick={() => setOpen((o) => !o)}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            setOpen((o) => !o);
          }
        }}
      >
        <span
          className={`${styles.caret} ${open ? styles.caretOpen : ""}`}
          aria-hidden
        >
          <CaretIcon />
        </span>
        <span className={styles.icon} aria-hidden>
          <FolderIcon />
        </span>
        <span className={styles.label}>{entry.name}</span>
      </div>
      {open && (
        <DirChildren path={entry.path} depth={depth + 1} filter={filter} />
      )}
    </>
  );
}

function DirChildren({
  path,
  depth,
  filter,
}: {
  path: string;
  depth: number;
  filter: string;
}) {
  const { data, isLoading, error } = useDir(path, true);
  if (isLoading) return <Meta depth={depth} label="Loading…" spinner />;
  if (error) return <Meta depth={depth} label="Failed to load" error />;
  const entries = data ?? [];
  if (entries.length === 0) return <Meta depth={depth} label="No .sql files" />;
  return (
    <>
      {entries.map((entry) =>
        entry.isDir ? (
          <DirNode
            key={entry.path}
            entry={entry}
            depth={depth}
            filter={filter}
          />
        ) : (
          <FileRow
            key={entry.path}
            entry={entry}
            depth={depth}
            filter={filter}
          />
        ),
      )}
    </>
  );
}

export function FileTree({
  folder,
  filter = "",
}: {
  folder: string;
  filter?: string;
}) {
  const [open, setOpen] = useState(true);
  return (
    <div className={styles.tree} role="tree">
      <div className={styles.rootRow}>
        <span
          className={`${styles.caret} ${open ? styles.caretOpen : ""}`}
          onClick={() => setOpen((o) => !o)}
          aria-hidden
        >
          <CaretIcon />
        </span>
        <span className={styles.icon} aria-hidden>
          <FolderIcon />
        </span>
        <span className={styles.rootName} title={folder}>
          {basename(folder)}
        </span>
        <button
          type="button"
          className="ghost"
          title="Remove folder"
          aria-label={`Remove folder ${basename(folder)}`}
          onClick={() => void closeFolder(folder)}
        >
          <CloseIcon />
        </button>
      </div>
      {open && <DirChildren path={folder} depth={1} filter={filter} />}
    </div>
  );
}

function Meta({
  depth,
  label,
  spinner,
  error,
}: {
  depth: number;
  label: string;
  spinner?: boolean;
  error?: boolean;
}) {
  return (
    <div
      className={`${styles.meta} ${error ? styles.error : ""}`}
      style={{ paddingLeft: depth * 14 + 22 }}
    >
      {spinner && <span className="spinner" aria-hidden />}
      {label}
    </div>
  );
}
