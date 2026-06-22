/**
 * SchemaTree lazy, per-level introspection + keyboard activation.
 *
 * The four introspection commands are mocked at the IPC boundary. We assert the
 * tree fetches nothing until a node is expanded, then fetches exactly the next
 * level on expand (databases -> schemas -> tables -> columns), and that tree
 * rows are activable by keyboard (Enter / Space), per the treeitem semantics.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactElement } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../ipc/commands", () => ({
  databasesList: vi.fn(),
  schemasList: vi.fn(),
  tablesList: vi.fn(),
  columnsList: vi.fn(),
}));

import {
  columnsList,
  databasesList,
  schemasList,
  tablesList,
} from "../ipc/commands";
import type { SessionInfo } from "../ipc/types";
import type { LiveSession } from "../state/sessionStore";
import { SchemaTree } from "./SchemaTree";

const mockDatabases = vi.mocked(databasesList);
const mockSchemas = vi.mocked(schemasList);
const mockTables = vi.mocked(tablesList);
const mockColumns = vi.mocked(columnsList);

const capabilities: SessionInfo["capabilities"] = {
  schemas: true,
  multiple_result_sets: true,
  server_side_cancel: true,
  transactions: true,
  explain_plan: false,
  streaming_rows: true,
  list_databases: true,
  data_editing: false,
};

const session: LiveSession = {
  info: { sessionId: "session-1", driver: "mssql", capabilities },
  connectionId: "c-1",
  connectionName: "Reporting DB",
  readOnly: false,
};

/** Render within a no-retry QueryClient so failures don't loop. */
function renderTree(ui: ReactElement) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>{ui}</QueryClientProvider>,
  );
}

beforeEach(() => {
  mockDatabases.mockReset();
  mockSchemas.mockReset();
  mockTables.mockReset();
  mockColumns.mockReset();

  mockDatabases.mockResolvedValue([
    { name: "AppDb", is_system: false, state_desc: "ONLINE" },
    { name: "master", is_system: true, state_desc: "ONLINE" },
  ]);
  mockSchemas.mockResolvedValue([{ name: "dbo" }, { name: "sales" }]);
  mockTables.mockResolvedValue([
    { schema: "dbo", name: "Customers", kind: "table" },
    { schema: "dbo", name: "ActiveOrders", kind: "view" },
  ]);
  mockColumns.mockResolvedValue([
    {
      name: "Id",
      ordinal: 0,
      data_type: "int",
      nullable: false,
      is_primary_key: true,
      max_length: null,
    },
  ]);
});

describe("SchemaTree lazy loading", () => {
  it("fetches the database list eagerly for the open server node", async () => {
    renderTree(<SchemaTree session={session} onDisconnect={vi.fn()} />);
    expect(await screen.findByText("AppDb")).toBeInTheDocument();
    expect(mockDatabases).toHaveBeenCalledWith("session-1");
    // Deeper levels are untouched until expansion.
    expect(mockSchemas).not.toHaveBeenCalled();
    expect(mockTables).not.toHaveBeenCalled();
    expect(mockColumns).not.toHaveBeenCalled();
  });

  it("expanding a database triggers schemas_list (and only that level)", async () => {
    const user = userEvent.setup();
    renderTree(<SchemaTree session={session} onDisconnect={vi.fn()} />);

    await user.click(await screen.findByText("AppDb"));

    await waitFor(() =>
      expect(mockSchemas).toHaveBeenCalledWith("session-1", "AppDb"),
    );
    expect(await screen.findByText("dbo")).toBeInTheDocument();
    // Tables are not fetched until a schema is expanded.
    expect(mockTables).not.toHaveBeenCalled();
  });

  it("expanding a schema triggers tables_list for that database+schema", async () => {
    const user = userEvent.setup();
    renderTree(<SchemaTree session={session} onDisconnect={vi.fn()} />);

    await user.click(await screen.findByText("AppDb"));
    await user.click(await screen.findByText("dbo"));

    await waitFor(() =>
      expect(mockTables).toHaveBeenCalledWith("session-1", "AppDb", "dbo"),
    );
    expect(await screen.findByText("Customers")).toBeInTheDocument();
    expect(mockColumns).not.toHaveBeenCalled();
  });

  it("activating tree rows with the keyboard (Enter) expands a node", async () => {
    const user = userEvent.setup();
    renderTree(<SchemaTree session={session} onDisconnect={vi.fn()} />);

    const dbRow = (await screen.findByText("AppDb")).closest(
      '[role="treeitem"]',
    ) as HTMLElement;
    dbRow.focus();
    await user.keyboard("{Enter}");

    await waitFor(() =>
      expect(mockSchemas).toHaveBeenCalledWith("session-1", "AppDb"),
    );
  });

  it("activating tree rows with the Space key also expands a node", async () => {
    const user = userEvent.setup();
    renderTree(<SchemaTree session={session} onDisconnect={vi.fn()} />);

    const dbRow = (await screen.findByText("AppDb")).closest(
      '[role="treeitem"]',
    ) as HTMLElement;
    dbRow.focus();
    await user.keyboard("[Space]");

    await waitFor(() =>
      expect(mockSchemas).toHaveBeenCalledWith("session-1", "AppDb"),
    );
  });

  it("disconnect button fires onDisconnect", async () => {
    const user = userEvent.setup();
    const onDisconnect = vi.fn();
    renderTree(<SchemaTree session={session} onDisconnect={onDisconnect} />);
    await user.click(
      screen.getByRole("button", { name: /Disconnect Reporting DB/ }),
    );
    expect(onDisconnect).toHaveBeenCalledTimes(1);
  });
});
