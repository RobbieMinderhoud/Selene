/**
 * ConnectionDialog behavior + the password-handling security contract.
 *
 * `connectionSave` / `connectionTest` are mocked at the IPC boundary, and the
 * toast store is mocked so no Tauri log/plugin is touched. We assert the saved
 * `ConnectionSpec` is shaped correctly, the read-only / trust-cert toggles flow
 * into it, and — critically — the password is only ever the *2nd argument* to
 * the IPC call: never inside the spec object, never rendered as visible text.
 */

import { render, screen, within, act } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../ipc/commands", () => ({
  connectionSave: vi.fn(),
  connectionTest: vi.fn(),
}));

vi.mock("../state/toastStore", () => ({
  toastError: vi.fn(),
  toastSuccess: vi.fn(),
}));

import { connectionSave, connectionTest } from "../ipc/commands";
import type { ConnectionSpec } from "../ipc/types";
import { ConnectionDialog } from "./ConnectionDialog";

const mockSave = vi.mocked(connectionSave);
const mockTest = vi.mocked(connectionTest);

const SECRET = "test-pw";

function renderDialog(
  over: Partial<Parameters<typeof ConnectionDialog>[0]> = {},
) {
  const props = {
    open: true,
    initial: null,
    onClose: vi.fn(),
    onSaved: vi.fn(),
    onSaveAndConnect: vi.fn(),
    ...over,
  };
  render(<ConnectionDialog {...props} />);
  return props;
}

beforeEach(() => {
  mockSave.mockReset();
  mockTest.mockReset();
  mockSave.mockResolvedValue({} as ConnectionSpec);
  mockTest.mockResolvedValue({ server_version: "16.0", elapsed_ms: 5 });
});

describe("ConnectionDialog", () => {
  it("renders all fields including the read-only and trust-cert toggles", () => {
    renderDialog();
    expect(screen.getByLabelText("Host")).toBeInTheDocument();
    expect(screen.getByLabelText("Port")).toBeInTheDocument();
    expect(screen.getByLabelText("Username")).toBeInTheDocument();
    expect(screen.getByLabelText("Password")).toBeInTheDocument();
    expect(
      screen.getByRole("checkbox", { name: /Read-only/ }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("checkbox", { name: /Trust server certificate/ }),
    ).toBeInTheDocument();
  });

  it("renders the password in a type=password field", () => {
    renderDialog();
    const pw = screen.getByLabelText("Password");
    expect(pw).toHaveAttribute("type", "password");
  });

  it("Save calls connectionSave with a correctly-shaped spec and the password as the 2nd arg", async () => {
    const user = userEvent.setup();
    renderDialog();

    await user.type(screen.getByLabelText("Name"), "Reporting DB");
    await user.type(screen.getByLabelText("Host"), "db.example.invalid");
    await user.type(screen.getByLabelText("Port"), "14333");
    await user.type(screen.getByLabelText("Username"), "report_reader");
    await user.type(screen.getByLabelText("Password"), SECRET);
    await user.click(screen.getByRole("checkbox", { name: /Read-only/ }));
    await user.click(
      screen.getByRole("checkbox", { name: /Trust server certificate/ }),
    );

    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(mockSave).toHaveBeenCalledTimes(1);
    const [spec, password] = mockSave.mock.calls[0];

    expect(spec).toMatchObject({
      name: "Reporting DB",
      driver: "mssql",
      host: "db.example.invalid",
      port: 14333,
      auth: { method: "sql_login", username: "report_reader" },
      tls: { encrypt: true, trust_server_certificate: true },
      read_only: true,
    });
    // The password is the second positional argument only.
    expect(password).toBe(SECRET);
  });

  it("never puts the password inside the saved spec object", async () => {
    const user = userEvent.setup();
    renderDialog();

    await user.type(screen.getByLabelText("Host"), "db.example.invalid");
    await user.type(screen.getByLabelText("Username"), "report_reader");
    await user.type(screen.getByLabelText("Password"), SECRET);
    await user.click(screen.getByRole("button", { name: "Save" }));

    const [spec] = mockSave.mock.calls[0];
    // No field of the spec, at any depth, equals the secret.
    expect(JSON.stringify(spec)).not.toContain(SECRET);
  });

  it("never renders the password as visible text in the DOM", async () => {
    const user = userEvent.setup();
    renderDialog();

    await user.type(screen.getByLabelText("Password"), SECRET);

    // The masked input holds the value, but it must not leak into text content.
    const pw = screen.getByLabelText("Password") as HTMLInputElement;
    expect(pw.value).toBe(SECRET);
    expect(document.body.textContent ?? "").not.toContain(SECRET);
  });

  it("Test calls connectionTest with the spec and password", async () => {
    const user = userEvent.setup();
    renderDialog();

    await user.type(screen.getByLabelText("Host"), "db.example.invalid");
    await user.type(screen.getByLabelText("Username"), "report_reader");
    await user.type(screen.getByLabelText("Password"), SECRET);
    await user.click(screen.getByRole("button", { name: "Test" }));

    expect(mockTest).toHaveBeenCalledTimes(1);
    const [spec, password] = mockTest.mock.calls[0];
    expect(spec).toMatchObject({ host: "db.example.invalid" });
    expect(password).toBe(SECRET);
    expect(JSON.stringify(spec)).not.toContain(SECRET);
  });

  it("validates required fields before calling IPC (no host -> no save)", async () => {
    const user = userEvent.setup();
    renderDialog();
    // Host left blank.
    await user.type(screen.getByLabelText("Username"), "report_reader");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(mockSave).not.toHaveBeenCalled();
  });

  it("omits the password argument entirely when left blank (unchanged-secret semantics)", async () => {
    const user = userEvent.setup();
    const initial: ConnectionSpec = {
      id: "c-1",
      name: "Existing",
      driver: "mssql",
      host: "db.example.invalid",
      port: 1433,
      instance: null,
      database: null,
      auth: { method: "sql_login", username: "report_reader" },
      tls: { encrypt: true, trust_server_certificate: false },
      read_only: false,
    };
    renderDialog({ initial });

    // Editing: password field starts blank; saving without typing one means
    // "leave the stored secret untouched" -> password arg is undefined.
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(mockSave).toHaveBeenCalledTimes(1);
    const [spec, password] = mockSave.mock.calls[0];
    expect(spec).toMatchObject({ id: "c-1", host: "db.example.invalid" });
    expect(password).toBeUndefined();
  });

  it("Save & Connect routes the saved spec to onSaveAndConnect", async () => {
    const user = userEvent.setup();
    const saved: ConnectionSpec = {
      id: "c-9",
      name: "Reporting DB",
      driver: "mssql",
      host: "db.example.invalid",
      port: null,
      instance: null,
      database: null,
      auth: { method: "sql_login", username: "report_reader" },
      tls: { encrypt: true, trust_server_certificate: false },
      read_only: false,
    };
    mockSave.mockResolvedValue(saved);
    const props = renderDialog();

    await user.type(screen.getByLabelText("Host"), "db.example.invalid");
    await user.type(screen.getByLabelText("Username"), "report_reader");
    await user.click(screen.getByRole("button", { name: "Save & Connect" }));

    expect(props.onSaveAndConnect).toHaveBeenCalledWith(saved);
    expect(props.onSaved).not.toHaveBeenCalled();
  });

  it("clears the form when reopened for a new connection after being closed", async () => {
    const user = userEvent.setup();
    const props = {
      open: true,
      initial: null as ConnectionSpec | null,
      onClose: vi.fn(),
      onSaved: vi.fn(),
      onSaveAndConnect: vi.fn(),
    };
    const { rerender } = render(<ConnectionDialog {...props} />);

    await user.type(screen.getByLabelText("Host"), "server1");
    await user.type(screen.getByLabelText("Username"), "admin");

    // Simulate closing and reopening for a new connection (same initial=null each time).
    act(() => rerender(<ConnectionDialog {...props} open={false} />));
    act(() => rerender(<ConnectionDialog {...props} open={true} />));

    expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("");
    expect((screen.getByLabelText("Username") as HTMLInputElement).value).toBe(
      "",
    );
  });

  it("Cancel invokes onClose without calling IPC", async () => {
    const user = userEvent.setup();
    const props = renderDialog();
    const footer = screen.getByRole("dialog");
    await user.click(within(footer).getByRole("button", { name: "Cancel" }));
    expect(props.onClose).toHaveBeenCalledTimes(1);
    expect(mockSave).not.toHaveBeenCalled();
    expect(mockTest).not.toHaveBeenCalled();
  });
});
