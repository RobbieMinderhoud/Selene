/**
 * CsvImportModal orchestration: pick → analyse → map → import.
 *
 * The OS file dialog, IPC commands, and the streaming channel are all mocked at
 * their module boundaries (no Tauri, no network). We drive the store with a
 * request and assert the modal analyses the file, renders the mapping menu, and
 * on Import calls `importCsv` with the exact target + mapping it built.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactElement } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: vi.fn(async () => "/tmp/data.csv"),
}));

vi.mock("../ipc/commands", () => ({
  importCsvAnalyze: vi.fn(),
  importCsv: vi.fn(),
  columnsList: vi.fn(),
}));

// Capture the channel callback (unused by these tests, but keeps the import
// path free of a real Tauri Channel).
vi.mock("../ipc/channels", () => ({
  createImportChannel: vi.fn(() => ({ __channel: true })),
}));

import { columnsList, importCsv, importCsvAnalyze } from "../ipc/commands";
import type { ColumnInfo, CsvAnalysis } from "../ipc/types";
import { useImportStore } from "../state/importStore";
import { CsvImportModal } from "./CsvImportModal";

const mockAnalyze = vi.mocked(importCsvAnalyze);
const mockImport = vi.mocked(importCsv);
const mockColumns = vi.mocked(columnsList);

function renderModal(ui: ReactElement) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>{ui}</QueryClientProvider>,
  );
}

const analysis: CsvAnalysis = {
  headers: ["id", "name"],
  sampleRows: [["1", "alice"]],
  inferred: [
    { sql_type: "INT", logical: "integer" },
    { sql_type: "NVARCHAR(50)", logical: "text" },
  ],
  rawPreview: ["id,name", "1,alice"],
};

beforeEach(() => {
  mockAnalyze.mockReset();
  mockImport.mockReset();
  mockColumns.mockReset();
  useImportStore.setState({ request: null });
});

afterEach(() => {
  useImportStore.setState({ request: null });
});

describe("CsvImportModal — new table", () => {
  it("analyses the picked file and imports with inferred columns + mapping", async () => {
    mockAnalyze.mockResolvedValue(analysis);
    mockImport.mockResolvedValue({ rows_inserted: 1, rows_skipped: 0 });

    useImportStore.getState().requestImport({
      sessionId: "s1",
      database: "db",
      schema: "dbo",
      mode: "new",
    });
    renderModal(<CsvImportModal />);

    // Analysis arrives → the per-column type selects render.
    await waitFor(() =>
      expect(screen.getByLabelText("Type for id")).toBeTruthy(),
    );
    expect(mockAnalyze).toHaveBeenCalledWith("/tmp/data.csv", {
      delimiter: ",",
      quote: '"',
      hasHeader: true,
    });

    // Name the table (set atomically — per-keystroke typing is flaky across the
    // controlled input's re-renders), then import.
    fireEvent.change(screen.getByLabelText("New table name"), {
      target: { value: "imported" },
    });
    await userEvent.click(screen.getByRole("button", { name: "Import" }));

    await waitFor(() => expect(mockImport).toHaveBeenCalledTimes(1));
    const [sessionId, path, target, mapping] = mockImport.mock.calls[0];
    expect(sessionId).toBe("s1");
    expect(path).toBe("/tmp/data.csv");
    expect(target).toEqual({
      kind: "new",
      database: "db",
      schema: "dbo",
      table: "imported",
      columns: [
        { name: "id", sqlType: "INT", nullable: true },
        { name: "name", sqlType: "NVARCHAR(50)", nullable: true },
      ],
    });
    expect(mapping).toEqual([
      { csvIndex: 0, targetColumn: "id" },
      { csvIndex: 1, targetColumn: "name" },
    ]);
  });
});

describe("CsvImportModal — existing table", () => {
  it("auto-matches CSV fields to columns by name and maps only matched ones", async () => {
    // CSV headers are ["id","name"]; the table has id, full_name, note.
    mockAnalyze.mockResolvedValue(analysis);
    const cols: ColumnInfo[] = [
      {
        name: "id",
        ordinal: 1,
        data_type: "int",
        nullable: true,
        is_primary_key: false,
        max_length: null,
      },
      {
        name: "name",
        ordinal: 2,
        data_type: "nvarchar",
        nullable: true,
        is_primary_key: false,
        max_length: 50,
      },
      {
        name: "note",
        ordinal: 3,
        data_type: "nvarchar",
        nullable: true,
        is_primary_key: false,
        max_length: 50,
      },
    ];
    mockColumns.mockResolvedValue(cols);
    mockImport.mockResolvedValue({ rows_inserted: 1, rows_skipped: 0 });

    useImportStore.getState().requestImport({
      sessionId: "s1",
      database: "db",
      schema: "dbo",
      mode: "existing",
      table: "people",
    });
    renderModal(<CsvImportModal />);

    // The per-column source selects render once analysis + columns resolve.
    await waitFor(() =>
      expect(screen.getByLabelText("CSV source for id")).toBeTruthy(),
    );
    // id and name auto-matched to CSV fields 0 and 1; note has no match.
    expect(
      (screen.getByLabelText("CSV source for id") as HTMLSelectElement).value,
    ).toBe("0");
    expect(
      (screen.getByLabelText("CSV source for name") as HTMLSelectElement).value,
    ).toBe("1");
    expect(
      (screen.getByLabelText("CSV source for note") as HTMLSelectElement).value,
    ).toBe("");

    await userEvent.click(screen.getByRole("button", { name: "Import" }));

    await waitFor(() => expect(mockImport).toHaveBeenCalledTimes(1));
    const [, , target, mapping] = mockImport.mock.calls[0];
    expect(target).toEqual({
      kind: "existing",
      database: "db",
      schema: "dbo",
      table: "people",
    });
    // Only the two matched columns are mapped; `note` is skipped entirely.
    expect(mapping).toEqual([
      { targetColumn: "id", csvIndex: 0 },
      { targetColumn: "name", csvIndex: 1 },
    ]);
  });
});
