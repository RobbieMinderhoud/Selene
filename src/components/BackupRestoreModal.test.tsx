/**
 * BackupModal / RestoreModal orchestration.
 *
 * The OS file dialog, IPC commands, and the streaming channels are mocked at
 * their module boundaries (no Tauri). We assert the dialogs gate their action
 * button correctly and call the right command with the args they built.
 */

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/plugin-dialog", () => ({
  save: vi.fn(async () => "/srv/backups/db.bak"),
  open: vi.fn(async () => "/srv/backups/source.bak"),
}));

vi.mock("../ipc/commands", () => ({
  databaseBackup: vi.fn(),
  databaseRestore: vi.fn(),
  restoreFilelist: vi.fn(),
  backupCancel: vi.fn(),
}));

vi.mock("../ipc/channels", () => ({
  createBackupChannel: vi.fn(() => ({ __channel: true })),
  createRestoreChannel: vi.fn(() => ({ __channel: true })),
}));

import {
  databaseBackup,
  databaseRestore,
  restoreFilelist,
} from "../ipc/commands";
import type { BackupFile } from "../ipc/types";
import { BackupModal } from "./BackupModal";
import { RestoreModal } from "./RestoreModal";

const mockBackup = vi.mocked(databaseBackup);
const mockRestore = vi.mocked(databaseRestore);
const mockFilelist = vi.mocked(restoreFilelist);

beforeEach(() => {
  mockBackup.mockReset();
  mockRestore.mockReset();
  mockFilelist.mockReset();
});

describe("BackupModal", () => {
  it("requires a destination, then backs up with the settings-default options", async () => {
    mockBackup.mockResolvedValue({ elapsedMs: 5, cancelled: false });
    const onClose = vi.fn();
    render(
      <BackupModal open sessionId="s1" database="Sales" onClose={onClose} />,
    );

    // "Back up" is disabled until a destination is chosen.
    const backUp = screen.getByRole("button", { name: "Back up" });
    expect(backUp).toBeDisabled();

    fireEvent.click(screen.getByRole("button", { name: "Choose…" }));
    await waitFor(() => expect(backUp).toBeEnabled());

    fireEvent.click(backUp);
    await waitFor(() => expect(mockBackup).toHaveBeenCalledTimes(1));
    const [sessionId, database, path, options] = mockBackup.mock.calls[0];
    expect(sessionId).toBe("s1");
    expect(database).toBe("Sales");
    expect(path).toBe("/srv/backups/db.bak");
    // Defaults from settingsStore: compression + checksum on, verify off.
    expect(options).toEqual({
      compression: true,
      checksum: true,
      verifyAfter: false,
    });
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });
});

describe("RestoreModal", () => {
  const files: BackupFile[] = [
    { logical_name: "Src", physical_name: "/old/Src.mdf", file_type: "D" },
    { logical_name: "Src_log", physical_name: "/old/Src.ldf", file_type: "L" },
  ];

  it("previews the backup files and gates restore behind typing the target name", async () => {
    mockFilelist.mockResolvedValue(files);
    mockRestore.mockResolvedValue({ elapsedMs: 9, cancelled: false });
    const onClose = vi.fn();
    const onRestored = vi.fn();
    render(
      <RestoreModal
        open
        sessionId="s1"
        target="Target"
        onClose={onClose}
        onRestored={onRestored}
      />,
    );

    const restore = screen.getByRole("button", { name: "Restore" });
    expect(restore).toBeDisabled();

    // Choose a .bak → its logical files are previewed via restore_filelist.
    fireEvent.click(screen.getByRole("button", { name: "Choose…" }));
    await waitFor(() =>
      expect(mockFilelist).toHaveBeenCalledWith(
        "s1",
        "/srv/backups/source.bak",
      ),
    );
    await screen.findByText("Src");
    expect(screen.getByText("Src_log")).toBeInTheDocument();

    // Still disabled until the target name is typed exactly.
    expect(restore).toBeDisabled();
    await userEvent.type(screen.getByPlaceholderText("Target"), "Target");
    await waitFor(() => expect(restore).toBeEnabled());

    fireEvent.click(restore);
    await waitFor(() => expect(mockRestore).toHaveBeenCalledTimes(1));
    const [sessionId, target, path, options] = mockRestore.mock.calls[0];
    expect(sessionId).toBe("s1");
    expect(target).toBe("Target");
    expect(path).toBe("/srv/backups/source.bak");
    expect(options).toEqual({ checksum: true });
    await waitFor(() => expect(onRestored).toHaveBeenCalled());
    expect(onClose).toHaveBeenCalled();
  });
});
