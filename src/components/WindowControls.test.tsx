/**
 * WindowControls: the Windows title-bar buttons drive the Tauri window API.
 *
 * `@tauri-apps/api/window` is mocked (no real window in jsdom); the test asserts
 * each button calls the matching window method and that the maximize button's
 * accessible name flips to "Restore" once the window reports it's maximized.
 */

import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

const win = vi.hoisted(() => ({
  minimize: vi.fn(),
  toggleMaximize: vi.fn(),
  close: vi.fn(),
  isMaximized: vi.fn().mockResolvedValue(false),
  onResized: vi.fn().mockResolvedValue(() => {}),
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => win,
}));

import { WindowControls } from "./WindowControls";

describe("WindowControls", () => {
  it("drives minimize / maximize / close through the window API", async () => {
    const user = userEvent.setup();
    render(<WindowControls />);

    await user.click(screen.getByRole("button", { name: "Minimize" }));
    expect(win.minimize).toHaveBeenCalledOnce();

    await user.click(screen.getByRole("button", { name: "Maximize" }));
    expect(win.toggleMaximize).toHaveBeenCalledOnce();

    await user.click(screen.getByRole("button", { name: "Close" }));
    expect(win.close).toHaveBeenCalledOnce();
  });

  it("shows Restore once the window reports it is maximized", async () => {
    win.isMaximized.mockResolvedValue(true);
    render(<WindowControls />);

    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Restore" })).toBeInTheDocument(),
    );
  });
});
