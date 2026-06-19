/**
 * `connectSession` + the password prompt it drives.
 *
 * `sessionConnect` is mocked at the IPC boundary; the prompt store and the
 * `PasswordPrompt` component are real. We assert the recovery flow end-to-end:
 * a `secret` error opens the prompt, a correct password retries with the typed
 * value (which the backend persists), a wrong password keeps the prompt open
 * with the error, and cancelling resolves the connect to `null`. Non-secret
 * errors propagate untouched (no prompt).
 */

import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../ipc/commands", () => ({
  sessionConnect: vi.fn(),
}));

import { sessionConnect } from "../ipc/commands";
import type { ConnectionSpec, SessionInfo } from "../ipc/types";
import { PasswordPrompt } from "../components/PasswordPrompt";
import { usePasswordPromptStore } from "../state/passwordPromptStore";
import { connectSession } from "./connect";

const mockConnect = vi.mocked(sessionConnect);

const SPEC = {
  id: "c1",
  name: "Prod DB",
  driver: "mssql",
  host: "db.example",
  port: 1433,
  instance: null,
  database: null,
  auth: { method: "sql_login", username: "sa" },
  tls: { encrypt: true, trust_server_certificate: false },
  read_only: false,
} as unknown as ConnectionSpec;

const INFO = {
  sessionId: "s1",
  driver: "mssql",
  capabilities: {},
} as unknown as SessionInfo;

const SECRET_ERR = { kind: "secret", message: "no stored password" };

beforeEach(() => {
  mockConnect.mockReset();
  usePasswordPromptStore.setState({ pending: null });
});

afterEach(() => {
  usePasswordPromptStore.setState({ pending: null });
});

describe("connectSession", () => {
  it("connects directly (no prompt) when a password is stored", async () => {
    mockConnect.mockResolvedValueOnce(INFO);
    const info = await connectSession("c1", SPEC);
    expect(info).toBe(INFO);
    expect(mockConnect).toHaveBeenCalledTimes(1);
    expect(mockConnect).toHaveBeenCalledWith("c1");
    expect(usePasswordPromptStore.getState().pending).toBeNull();
  });

  it("propagates a non-secret error without prompting", async () => {
    mockConnect.mockRejectedValueOnce({
      kind: "driver",
      message: "unreachable",
    });
    await expect(connectSession("c1", SPEC)).rejects.toMatchObject({
      kind: "driver",
    });
    expect(usePasswordPromptStore.getState().pending).toBeNull();
  });

  it("prompts on a secret error, then connects with the typed password", async () => {
    render(<PasswordPrompt />);
    mockConnect.mockRejectedValueOnce(SECRET_ERR).mockResolvedValueOnce(INFO);

    const promise = connectSession("c1", SPEC);

    const input = await screen.findByLabelText("Password");
    await userEvent.type(input, "hunter2");
    await userEvent.click(screen.getByRole("button", { name: "Connect" }));

    await expect(promise).resolves.toBe(INFO);
    // Second call carries the password so the backend can persist it.
    expect(mockConnect).toHaveBeenNthCalledWith(2, "c1", "hunter2");
  });

  it("keeps the prompt open with the error after a wrong password", async () => {
    render(<PasswordPrompt />);
    mockConnect
      .mockRejectedValueOnce(SECRET_ERR)
      .mockRejectedValueOnce({
        kind: "driver",
        message: "Login failed for user.",
      })
      .mockResolvedValueOnce(INFO);

    const promise = connectSession("c1", SPEC);

    const input = await screen.findByLabelText("Password");
    await userEvent.type(input, "wrong");
    await userEvent.click(screen.getByRole("button", { name: "Connect" }));

    // Error surfaced inline; prompt still open for another try.
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Login failed for user.",
    );
    expect(usePasswordPromptStore.getState().pending).not.toBeNull();

    await userEvent.clear(input);
    await userEvent.type(input, "right");
    await userEvent.click(screen.getByRole("button", { name: "Connect" }));

    await expect(promise).resolves.toBe(INFO);
    expect(mockConnect).toHaveBeenNthCalledWith(3, "c1", "right");
  });

  it("resolves null when the prompt is cancelled", async () => {
    render(<PasswordPrompt />);
    mockConnect.mockRejectedValueOnce(SECRET_ERR);

    const promise = connectSession("c1", SPEC);

    await screen.findByLabelText("Password");
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));

    await expect(promise).resolves.toBeNull();
    // Only the initial (stored-password) attempt happened — no retry.
    expect(mockConnect).toHaveBeenCalledTimes(1);
  });
});
