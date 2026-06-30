/**
 * BackupModal / RestoreModal orchestration.
 *
 * The IPC commands and streaming channels are mocked at their module
 * boundaries (no Tauri). Backup/restore paths are server-side: the dialogs use
 * an editable path field (+ a Browse button backed by `server_list_dir`). We
 * assert the action button is gated correctly and calls the right command with
 * the path/options it built.
 */

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../ipc/commands", () => ({
  databaseBackup: vi.fn(),
  databaseRestore: vi.fn(),
  restoreFilelist: vi.fn(),
  backupCancel: vi.fn(),
  serverDefaultBackupDir: vi.fn(async () => ""),
  serverListDir: vi.fn(async () => []),
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
  it("backs up to the typed server path with the settings-default options", async () => {
    mockBackup.mockResolvedValue({ elapsedMs: 5, cancelled: false });
    const onClose = vi.fn();
    render(
      <BackupModal open sessionId="s1" database="Sales" onClose={onClose} />,
    );

    const dest = screen.getByLabelText("Backup destination path");
    await userEvent.clear(dest);
    await userEvent.type(dest, "/mnt/backups/db.bak");

    fireEvent.click(screen.getByRole("button", { name: "Back up" }));
    await waitFor(() => expect(mockBackup).toHaveBeenCalledTimes(1));
    const [sessionId, database, path, options] = mockBackup.mock.calls[0];
    expect(sessionId).toBe("s1");
    expect(database).toBe("Sales");
    expect(path).toBe("/mnt/backups/db.bak");
    // Defaults from settingsStore: compression + checksum on, verify off.
    expect(options).toEqual({
      compression: true,
      checksum: true,
      verifyAfter: false,
    });
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });

  it("disables Back up until a destination is set", async () => {
    render(
      <BackupModal open sessionId="s1" database="Sales" onClose={vi.fn()} />,
    );
    const dest = screen.getByLabelText("Backup destination path");
    await userEvent.clear(dest);
    expect(screen.getByRole("button", { name: "Back up" })).toBeDisabled();
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

    // Type a server path; blur loads the file list via restore_filelist.
    const src = screen.getByLabelText("Backup file path");
    await userEvent.type(src, "/mnt/backups/source.bak");
    fireEvent.blur(src);
    await waitFor(() =>
      expect(mockFilelist).toHaveBeenCalledWith(
        "s1",
        "/mnt/backups/source.bak",
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
    expect(path).toBe("/mnt/backups/source.bak");
    expect(options).toEqual({ checksum: true });
    await waitFor(() => expect(onRestored).toHaveBeenCalled());
    expect(onClose).toHaveBeenCalled();
  });
});
