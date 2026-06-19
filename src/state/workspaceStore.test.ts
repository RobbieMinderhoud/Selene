/**
 * Workspace manifest: serialization contract + parse guards.
 *
 * The manifest is what reopens saved tabs/folders next launch. We verify it
 * captures only file-backed tabs (never scratch tabs / SQL content), round-trips
 * through localStorage, and tolerates absent/garbage storage.
 */

import { beforeEach, describe, expect, it } from "vitest";

import { useEditorStore } from "./editorStore";
import {
  currentManifest,
  readWorkspace,
  useWorkspaceStore,
} from "./workspaceStore";

const STORAGE_KEY = "selene.workspace";

beforeEach(() => {
  useEditorStore.setState({ tabs: [], activeTabId: null, results: {} });
  useWorkspaceStore.setState({ openFolders: [] });
  localStorage.clear();
});

describe("readWorkspace", () => {
  it("returns an empty manifest when nothing is stored", () => {
    expect(readWorkspace()).toEqual({
      openFolders: [],
      openFiles: [],
      activeFile: null,
    });
  });

  it("returns an empty manifest on malformed JSON", () => {
    localStorage.setItem(STORAGE_KEY, "{not json");
    expect(readWorkspace()).toEqual({
      openFolders: [],
      openFiles: [],
      activeFile: null,
    });
  });

  it("filters out non-string entries and a non-string activeFile", () => {
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        openFolders: ["/a", 42, null, "/b"],
        openFiles: ["/a/x.sql", {}],
        activeFile: 7,
      }),
    );
    expect(readWorkspace()).toEqual({
      openFolders: ["/a", "/b"],
      openFiles: ["/a/x.sql"],
      activeFile: null,
    });
  });
});

describe("currentManifest", () => {
  it("captures only file-backed tabs, the active file, and folders", () => {
    const ed = useEditorStore.getState();
    ed.addTab(null, "scratch -- not persisted");
    const fileId = ed.openFileTab("/work/report.sql", "SELECT 1;");
    useWorkspaceStore.getState().addFolder("/work");

    const m = currentManifest();
    expect(m.openFiles).toEqual(["/work/report.sql"]); // scratch tab excluded
    expect(m.openFolders).toEqual(["/work"]);
    expect(m.activeFile).toBe("/work/report.sql"); // openFileTab focused it

    // Switching to the scratch tab makes activeFile null (no path).
    const scratch = useEditorStore
      .getState()
      .tabs.find((t) => t.id !== fileId)!;
    useEditorStore.getState().setActiveTab(scratch.id);
    expect(currentManifest().activeFile).toBeNull();
  });

  it("round-trips through localStorage", () => {
    useEditorStore.getState().openFileTab("/work/a.sql", "SELECT 1;");
    useEditorStore.getState().openFileTab("/work/sub/b.sql", "SELECT 2;");
    useWorkspaceStore.getState().addFolder("/work");

    const before = currentManifest();
    localStorage.setItem(STORAGE_KEY, JSON.stringify(before));
    expect(readWorkspace()).toEqual(before);
  });
});

describe("useWorkspaceStore folder actions", () => {
  it("addFolder is idempotent; removeFolder drops it", () => {
    const s = () => useWorkspaceStore.getState();
    s().addFolder("/work");
    s().addFolder("/work");
    expect(s().openFolders).toEqual(["/work"]);
    s().addFolder("/other");
    expect(s().openFolders).toEqual(["/work", "/other"]);
    s().removeFolder("/work");
    expect(s().openFolders).toEqual(["/other"]);
  });
});
