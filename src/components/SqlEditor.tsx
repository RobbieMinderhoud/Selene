/**
 * CodeMirror 6 SQL editor for the active tab.
 *
 * Render isolation: this component selects ONLY `tabs[id].sql` from the store
 * (via `selectSql`), so the high-frequency `rows` appends that bump a result's
 * `rev` never touch it. The CodeMirror value is driven by that single string.
 *
 * Keybinding: Cmd/Ctrl+Enter runs the current selection if there is one, else
 * the whole document. The run itself is delegated to `onRun(sql)` so the editor
 * stays free of guard/stream logic.
 */

import { useMemo, useRef, useState } from "react";
import CodeMirror from "@uiw/react-codemirror";
import { sql, MSSQL } from "@codemirror/lang-sql";
import type { CompletionSource } from "@codemirror/autocomplete";
import { EditorView, keymap } from "@codemirror/view";
import { Prec } from "@codemirror/state";
import { indentUnit } from "@codemirror/language";
import { search } from "@codemirror/search";
import { githubDark, githubLight } from "@uiw/codemirror-theme-github";

import { selectSql, useEditorStore } from "../state/editorStore";
import { useThemeStore } from "../state/themeStore";
import { useSettingsStore } from "../state/settingsStore";
import { EditorSearch } from "./EditorSearch";
import styles from "./SqlEditor.module.css";

interface SqlEditorProps {
  tabId: string;
  /** Run a SQL string (selection or whole doc). */
  onRun: (sql: string) => void;
  /**
   * Schema-aware completion source for the tab's active connection/database.
   * Registered alongside lang-sql's keyword completion. Must be referentially
   * stable across keystrokes (memoized by the caller) or the editor will
   * reconfigure on every character.
   */
  schemaSource?: CompletionSource;
}

export function SqlEditor({ tabId, onRun, schemaSource }: SqlEditorProps) {
  const value = useEditorStore((s) => selectSql(s, tabId));
  const setSql = useEditorStore((s) => s.setSql);
  const themeMode = useThemeStore((s) => s.mode);
  const editor = useSettingsStore((s) => s.editor);

  // The live view, captured on creation, so the find/replace overlay can drive
  // the search query API against it.
  const [view, setView] = useState<EditorView | null>(null);
  const [find, setFind] = useState({ open: false, replace: false });
  // A stable handle the editor keymap calls to open the overlay; kept in a ref so
  // the (memoized) extensions never rebuild when this closure changes per render.
  const openFind = (replace: boolean) => setFind({ open: true, replace });
  const openFindRef = useRef(openFind);
  openFindRef.current = openFind;

  // githubLight hard-codes a white background via its own EditorView.theme().
  // For the retro palette we need to punch through that with Prec.highest so
  // the warm cream colours win without replacing the syntax highlighting.
  const retroBg = useMemo(
    () =>
      themeMode === "retro"
        ? [
            Prec.highest(
              EditorView.theme({
                "&": { background: "#fbf1c7" },
                ".cm-gutters": {
                  background: "#ebdbb2",
                  borderRight: "1px solid #bdae93",
                  color: "#a89984",
                },
                ".cm-activeLine": { background: "rgba(213,196,161,0.35)" },
                ".cm-activeLineGutter": { background: "#d5c4a1" },
              }),
            ),
          ]
        : [],
    [themeMode],
  );

  // Extensions rebuilt when run binding or editor settings change.
  const extensions = useMemo(() => {
    const editorKeymap = Prec.highest(
      keymap.of([
        {
          key: "Mod-Enter",
          preventDefault: true,
          run: (view) => {
            const state = view.state;
            const sel = state.selection.main;
            const text = sel.empty
              ? state.doc.toString()
              : state.sliceDoc(sel.from, sel.to);
            onRun(text);
            return true;
          },
        },
        // Open our themed find/replace overlay instead of CodeMirror's stock
        // panel (the default Mod-f binding is suppressed via `searchKeymap:false`).
        {
          key: "Mod-f",
          preventDefault: true,
          run: () => {
            openFindRef.current(false);
            return true;
          },
        },
        {
          key: "Mod-Alt-f",
          preventDefault: true,
          run: () => {
            openFindRef.current(true);
            return true;
          },
        },
      ]),
    );
    const exts = [
      sql({ dialect: MSSQL, upperCaseKeywords: editor.upperCaseKeywords }),
      editorKeymap,
      // Provides the search state the overlay drives via `setSearchQuery`; its
      // own panel is never opened, so only the query/highlight machinery is used.
      search(),
      indentUnit.of(" ".repeat(editor.tabSize)),
      EditorView.theme({
        "&": { height: "100%", fontSize: `${editor.fontSize}px` },
        ".cm-scroller": {
          fontFamily: "var(--font-mono)",
          lineHeight: "1.55",
        },
        ".cm-content": { paddingBlock: "8px" },
        // Theme the search highlights with our accent so matches read on every
        // palette (the stock yellow clashes with dark/retro).
        ".cm-searchMatch": {
          backgroundColor: "color-mix(in srgb, var(--accent) 26%, transparent)",
          borderRadius: "2px",
        },
        ".cm-searchMatch-selected": {
          backgroundColor: "color-mix(in srgb, var(--accent) 50%, transparent)",
          outline: "1px solid var(--accent)",
        },
      }),
    ];
    // Register schema/column completion as an extra source on the SQL language's
    // autocomplete facet (the same channel lang-sql uses for its keyword + schema
    // sources), so the single basicSetup `autocompletion()` plugin reads both.
    if (schemaSource) {
      exts.push(MSSQL.language.data.of({ autocomplete: schemaSource }));
    }
    if (editor.wordWrap) exts.push(EditorView.lineWrapping);
    return exts;
  }, [
    onRun,
    schemaSource,
    editor.upperCaseKeywords,
    editor.fontSize,
    editor.tabSize,
    editor.wordWrap,
  ]);

  // Stabilize the final extension array so it only changes when `extensions` or
  // `retroBg` change — not on every keystroke-driven re-render (each new array
  // identity triggers a full CodeMirror reconfigure).
  const allExtensions = useMemo(
    () => [...extensions, ...retroBg],
    [extensions, retroBg],
  );

  return (
    <div className={styles.host}>
      <CodeMirror
        value={value}
        height="100%"
        theme={themeMode === "dark" ? githubDark : githubLight}
        extensions={allExtensions}
        onChange={(next) => setSql(tabId, next)}
        onCreateEditor={(v) => setView(v)}
        basicSetup={{
          lineNumbers: editor.lineNumbers,
          foldGutter: false,
          highlightActiveLine: true,
          autocompletion: editor.autocompletion,
          bracketMatching: editor.bracketPairs,
          closeBrackets: editor.bracketPairs,
          // The stock Mod-f search panel is replaced by our themed overlay.
          searchKeymap: false,
        }}
        style={{ height: "100%", overflow: "hidden" }}
      />
      <EditorSearch
        view={view}
        open={find.open}
        replaceMode={find.replace}
        onReplaceModeChange={(replace) => setFind((f) => ({ ...f, replace }))}
        onClose={() => setFind((f) => ({ ...f, open: false }))}
      />
    </div>
  );
}
