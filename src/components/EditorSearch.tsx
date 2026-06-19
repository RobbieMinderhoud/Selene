/**
 * Themed find/replace overlay for the SQL editor.
 *
 * Replaces CodeMirror's stock search panel (which ignores our tokens and motion)
 * with a floating React panel driven by the `@codemirror/search` query API:
 * `setSearchQuery` publishes the query into the editor state, and the
 * `findNext`/`findPrevious`/`replaceNext`/`replaceAll` commands act on it. The
 * panel itself never touches the document directly — it only configures the query
 * and invokes commands, so highlighting/selection stays the editor's job.
 *
 * Motion: mounts/unmounts through `usePresence` so it animates both in and out;
 * the CSS keys off `data-state`. Toggle states (case/regex/whole-word) persist
 * via `settingsStore` so the panel reopens the way the user left it.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { EditorView } from "@codemirror/view";
import {
  SearchQuery,
  setSearchQuery,
  findNext,
  findPrevious,
  replaceNext,
  replaceAll,
} from "@codemirror/search";

import { MOTION, usePresence } from "../lib/motion";
import { useSettingsStore } from "../state/settingsStore";
import {
  CaretIcon,
  CloseIcon,
  MatchCaseIcon,
  NextMatchIcon,
  PrevMatchIcon,
  RegexIcon,
  ReplaceAllIcon,
  ReplaceIcon,
  SearchIcon,
  WholeWordIcon,
} from "./icons";
import styles from "./EditorSearch.module.css";

interface EditorSearchProps {
  /** The live CodeMirror view, or null before the editor mounts. */
  view: EditorView | null;
  open: boolean;
  /** Whether the replace row is expanded. */
  replaceMode: boolean;
  onReplaceModeChange: (next: boolean) => void;
  onClose: () => void;
}

interface Stats {
  total: number;
  /** 1-based index of the match under the current selection, or 0 if none. */
  current: number;
  /** Regex was requested but does not compile. */
  invalid: boolean;
}

const EMPTY_STATS: Stats = { total: 0, current: 0, invalid: false };

/** Walk all matches to derive a total and the 1-based index of the selected one. */
function computeStats(view: EditorView, query: SearchQuery): Stats {
  if (!query.search || !query.valid) {
    return { total: 0, current: 0, invalid: query.regexp && !!query.search };
  }
  const sel = view.state.selection.main;
  const cursor = query.getCursor(view.state);
  let total = 0;
  let current = 0;
  let res = cursor.next();
  while (!res.done) {
    total++;
    if (res.value.from === sel.from && res.value.to === sel.to) current = total;
    // Cap the scan so a pathological pattern on a huge doc can't lock the UI.
    if (total >= 10_000) break;
    res = cursor.next();
  }
  return { total, current, invalid: false };
}

export function EditorSearch({
  view,
  open,
  replaceMode,
  onReplaceModeChange,
  onClose,
}: EditorSearchProps) {
  const { mounted, state } = usePresence(open, MOTION.fast);

  const opts = useSettingsStore((s) => s.search);
  const setSettings = useSettingsStore((s) => s.set);

  const [query, setQuery] = useState("");
  const [replace, setReplace] = useState("");
  const [stats, setStats] = useState<Stats>(EMPTY_STATS);

  const searchRef = useRef<HTMLInputElement>(null);

  const buildQuery = useCallback(
    () =>
      new SearchQuery({
        search: query,
        replace,
        caseSensitive: opts.caseSensitive,
        regexp: opts.regexp,
        wholeWord: opts.wholeWord,
        // When not in regex mode, treat \n \t etc. literally so a user typing a
        // backslash searches for a backslash rather than an escape sequence.
        literal: !opts.regexp,
      }),
    [query, replace, opts.caseSensitive, opts.regexp, opts.wholeWord],
  );

  // Publish the query into the editor whenever it (or a toggle) changes while
  // open. Gated on `open` so the close-cleanup effect isn't immediately undone.
  useEffect(() => {
    if (!open || !view) return;
    const q = buildQuery();
    view.dispatch({ effects: setSearchQuery.of(q) });
    setStats(computeStats(view, q));
  }, [open, view, buildQuery]);

  // On open: seed from the current selection (single-line only) and focus.
  useEffect(() => {
    if (!open || !view) return;
    const sel = view.state.selection.main;
    if (!sel.empty) {
      const text = view.state.sliceDoc(sel.from, sel.to);
      if (text && !text.includes("\n")) setQuery(text);
    }
    const id = requestAnimationFrame(() => {
      searchRef.current?.focus();
      searchRef.current?.select();
    });
    return () => cancelAnimationFrame(id);
  }, [open, view]);

  // On close: drop the highlight and hand focus back to the editor.
  useEffect(() => {
    if (open || !view) return;
    view.dispatch({
      effects: setSearchQuery.of(new SearchQuery({ search: "" })),
    });
    view.focus();
  }, [open, view]);

  // Run a search command, then recompute the match counter from the new state.
  const run = useCallback(
    (cmd: (v: EditorView) => boolean) => {
      if (!view) return;
      cmd(view);
      setStats(computeStats(view, buildQuery()));
    },
    [view, buildQuery],
  );

  const toggle = useCallback(
    (key: keyof typeof opts) => setSettings("search", { [key]: !opts[key] }),
    [opts, setSettings],
  );

  const statusText = useMemo(() => {
    if (!query) return "";
    if (stats.invalid) return "Bad pattern";
    if (stats.total === 0) return "No results";
    return stats.current > 0
      ? `${stats.current} of ${stats.total}`
      : `${stats.total} found`;
  }, [query, stats]);

  if (!mounted) return null;

  const noMatches = !!query && (stats.invalid || stats.total === 0);

  return (
    <div
      className={styles.panel}
      data-state={state}
      role="search"
      aria-label="Find in editor"
    >
      <button
        type="button"
        className={`ghost ${styles.expand}`}
        aria-label={replaceMode ? "Hide replace" : "Show replace"}
        aria-expanded={replaceMode}
        title={replaceMode ? "Hide replace" : "Show replace"}
        onClick={() => onReplaceModeChange(!replaceMode)}
      >
        <CaretIcon
          className={styles.expandCaret}
          data-open={replaceMode || undefined}
        />
      </button>

      <div className={styles.rows}>
        <div className={styles.row}>
          <div
            className={`${styles.field} ${stats.invalid ? styles.fieldError : ""}`}
          >
            <SearchIcon className={styles.fieldIcon} />
            <input
              ref={searchRef}
              className={styles.input}
              placeholder="Find"
              value={query}
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  run(e.shiftKey ? findPrevious : findNext);
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  onClose();
                }
              }}
            />
            <div className={styles.toggles}>
              <button
                type="button"
                className={`${styles.toggle} ${opts.caseSensitive ? styles.toggleOn : ""}`}
                aria-pressed={opts.caseSensitive}
                title="Match case"
                onClick={() => toggle("caseSensitive")}
              >
                <MatchCaseIcon />
              </button>
              <button
                type="button"
                className={`${styles.toggle} ${opts.wholeWord ? styles.toggleOn : ""}`}
                aria-pressed={opts.wholeWord}
                title="Whole word"
                onClick={() => toggle("wholeWord")}
              >
                <WholeWordIcon />
              </button>
              <button
                type="button"
                className={`${styles.toggle} ${opts.regexp ? styles.toggleOn : ""}`}
                aria-pressed={opts.regexp}
                title="Use regular expression"
                onClick={() => toggle("regexp")}
              >
                <RegexIcon />
              </button>
            </div>
          </div>

          <span
            className={`${styles.count} ${noMatches ? styles.countEmpty : ""}`}
            aria-live="polite"
          >
            {statusText}
          </span>

          <div className={styles.nav}>
            <button
              type="button"
              className="ghost"
              title="Previous match (Shift+Enter)"
              aria-label="Previous match"
              disabled={stats.total === 0}
              onClick={() => run(findPrevious)}
            >
              <PrevMatchIcon />
            </button>
            <button
              type="button"
              className="ghost"
              title="Next match (Enter)"
              aria-label="Next match"
              disabled={stats.total === 0}
              onClick={() => run(findNext)}
            >
              <NextMatchIcon />
            </button>
            <button
              type="button"
              className="ghost"
              title="Close (Esc)"
              aria-label="Close find"
              onClick={onClose}
            >
              <CloseIcon />
            </button>
          </div>
        </div>

        {replaceMode && (
          <div className={`${styles.row} ${styles.replaceRow}`}>
            <div className={styles.field}>
              <ReplaceIcon className={styles.fieldIcon} />
              <input
                className={styles.input}
                placeholder="Replace"
                value={replace}
                spellCheck={false}
                autoComplete="off"
                onChange={(e) => setReplace(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    run(
                      e.metaKey || e.ctrlKey || e.altKey
                        ? replaceAll
                        : replaceNext,
                    );
                  } else if (e.key === "Escape") {
                    e.preventDefault();
                    onClose();
                  }
                }}
              />
            </div>
            <div className={styles.nav}>
              <button
                type="button"
                className="ghost"
                title="Replace (Enter)"
                aria-label="Replace"
                disabled={stats.total === 0}
                onClick={() => run(replaceNext)}
              >
                <ReplaceIcon />
              </button>
              <button
                type="button"
                className="ghost"
                title="Replace all (Cmd/Ctrl+Enter)"
                aria-label="Replace all"
                disabled={stats.total === 0}
                onClick={() => run(replaceAll)}
              >
                <ReplaceAllIcon />
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
