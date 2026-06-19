/**
 * Toasts: renders a toast's message + optional detail and can be dismissed.
 *
 * Driven through the real toast store via `push` (sticky, so no auto-dismiss
 * timer fires during the test) and `dismiss`.
 */

import {
  render,
  screen,
  waitForElementToBeRemoved,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { act } from "react";
import { afterEach, describe, expect, it } from "vitest";

import { useToastStore } from "../state/toastStore";
import { Toasts } from "./Toasts";

afterEach(() => {
  // Clear any toasts left over so each test starts from an empty stack.
  useToastStore.setState({ toasts: [] });
});

describe("Toasts", () => {
  it("renders nothing when the stack is empty", () => {
    const { container } = render(<Toasts />);
    expect(container).toBeEmptyDOMElement();
  });

  it("renders a toast's message and detail line", () => {
    render(<Toasts />);
    act(() => {
      useToastStore.getState().push({
        kind: "error",
        message: "Query failed",
        detail: "syntax error",
        sticky: true,
      });
    });
    expect(screen.getByText("Query failed")).toBeInTheDocument();
    expect(screen.getByText("syntax error")).toBeInTheDocument();
    // Error toasts are announced assertively.
    expect(screen.getByRole("alert")).toBeInTheDocument();
  });

  it("uses role=status for non-error toasts", () => {
    render(<Toasts />);
    act(() => {
      useToastStore
        .getState()
        .push({ kind: "success", message: "Connection saved.", sticky: true });
    });
    expect(screen.getByRole("status")).toHaveTextContent("Connection saved.");
  });

  it("dismisses a toast when its dismiss button is clicked", async () => {
    const user = userEvent.setup();
    render(<Toasts />);
    act(() => {
      useToastStore
        .getState()
        .push({ kind: "info", message: "Exporting…", sticky: true });
    });
    const toast = screen.getByText("Exporting…");
    expect(toast).toBeInTheDocument();

    await user.click(
      screen.getByRole("button", { name: "Dismiss notification" }),
    );
    // Dismissal is animated: the toast first enters its exit ("closed") state,
    // then unmounts once the exit animation has run.
    expect(toast.closest('[data-state="closed"]')).not.toBeNull();
    await waitForElementToBeRemoved(() => screen.queryByText("Exporting…"));
  });
});
