/**
 * Settings store — the `import` section.
 *
 * The store serialises an *explicit* list of sections to localStorage in two
 * places (`set` and `importSettings`); a new section is easy to add to the type
 * but forget in those blocks, which would silently fail to persist. These tests
 * pin that the `import` section round-trips and deep-merges over defaults.
 */

import { beforeEach, describe, expect, it } from "vitest";

import { useSettingsStore } from "./settingsStore";

const KEY = "selene.settings";

function stored(): Record<string, unknown> {
  return JSON.parse(localStorage.getItem(KEY) ?? "{}");
}

beforeEach(() => {
  localStorage.clear();
  useSettingsStore.getState().resetSettings();
});

describe("settingsStore — import section", () => {
  it("has sensible defaults", () => {
    expect(useSettingsStore.getState().import).toEqual({
      delimiter: ",",
      quoteChar: '"',
      hasHeader: true,
      emptyAsNull: true,
      atomic: true,
    });
  });

  it("persists the import section to localStorage on set", () => {
    useSettingsStore
      .getState()
      .set("import", { delimiter: ";", atomic: false });

    expect(useSettingsStore.getState().import.delimiter).toBe(";");
    expect(useSettingsStore.getState().import.atomic).toBe(false);
    // The serialised blob MUST include the import section (the two-block gotcha).
    expect(stored().import).toEqual({
      delimiter: ";",
      quoteChar: '"',
      hasHeader: true,
      emptyAsNull: true,
      atomic: false,
    });
  });

  it("deep-merges import over defaults on importSettings (missing keys filled)", () => {
    useSettingsStore.getState().importSettings({ import: { delimiter: "\t" } });

    const imp = useSettingsStore.getState().import;
    expect(imp.delimiter).toBe("\t");
    expect(imp.hasHeader).toBe(true); // default preserved
    expect((stored().import as { delimiter: string }).delimiter).toBe("\t");
  });

  it("fills the import section from defaults when a saved blob predates it", () => {
    // A backup that has no `import` key at all must not leave it undefined.
    useSettingsStore.getState().importSettings({ export: { delimiter: "," } });
    expect(useSettingsStore.getState().import.delimiter).toBe(",");
    expect(useSettingsStore.getState().import.atomic).toBe(true);
  });
});

describe("settingsStore — multiTarget section", () => {
  it("has sensible defaults", () => {
    const mt = useSettingsStore.getState().multiTarget;
    expect(mt.maxParallelServers).toBe(4);
    expect(mt.csvDelimiter).toBe(";");
    expect(mt.csvBom).toBe(false);
    expect(mt.defaultFilterQuery).toContain("sys.databases");
  });

  it("persists the multiTarget section to localStorage on set", () => {
    useSettingsStore
      .getState()
      .set("multiTarget", { maxParallelServers: 8, csvBom: true });

    expect(useSettingsStore.getState().multiTarget.maxParallelServers).toBe(8);
    // The serialised blob MUST include the multiTarget section (two-block gotcha).
    expect(
      (stored().multiTarget as { maxParallelServers: number })
        .maxParallelServers,
    ).toBe(8);
    expect((stored().multiTarget as { csvBom: boolean }).csvBom).toBe(true);
  });

  it("deep-merges multiTarget over defaults on importSettings", () => {
    useSettingsStore
      .getState()
      .importSettings({ multiTarget: { maxParallelServers: 2 } });
    const mt = useSettingsStore.getState().multiTarget;
    expect(mt.maxParallelServers).toBe(2);
    expect(mt.csvDelimiter).toBe(";"); // default preserved
  });

  it("fills the multiTarget section from defaults when a saved blob predates it", () => {
    useSettingsStore.getState().importSettings({ export: { delimiter: "," } });
    expect(useSettingsStore.getState().multiTarget.maxParallelServers).toBe(4);
  });
});
