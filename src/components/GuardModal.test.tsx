/**
 * GuardModal: renders verdict reasons and resolves Confirm / Cancel correctly,
 * for both the `confirm` (Run anyway / Cancel) and `block` (OK) variants.
 */

import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { GuardModal } from "./GuardModal";

describe("GuardModal", () => {
  it("renders nothing when there is no pending verdict", () => {
    const { container } = render(
      <GuardModal state={null} onConfirm={vi.fn()} onCancel={vi.fn()} />,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it("confirm: lists the reasons and offers Run anyway / Cancel", async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    render(
      <GuardModal
        state={{
          kind: "confirm",
          verdict: { level: "confirm", reasons: ["DELETE without WHERE"] },
        }}
        onConfirm={onConfirm}
        onCancel={onCancel}
      />,
    );

    expect(screen.getByText("DELETE without WHERE")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Run anyway" }));
    expect(onConfirm).toHaveBeenCalledTimes(1);
    expect(onCancel).not.toHaveBeenCalled();
  });

  it("confirm: Cancel resolves via onCancel", async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    render(
      <GuardModal
        state={{
          kind: "confirm",
          verdict: { level: "confirm", reasons: ["TRUNCATE TABLE"] },
        }}
        onConfirm={onConfirm}
        onCancel={onCancel}
      />,
    );
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onCancel).toHaveBeenCalledTimes(1);
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("block: shows the reasons and only an OK (cancel) action", async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    render(
      <GuardModal
        state={{
          kind: "block",
          verdict: {
            level: "block",
            reasons: ["non-SELECT on a read-only connection"],
          },
        }}
        onConfirm={onConfirm}
        onCancel={onCancel}
      />,
    );

    expect(
      screen.getByText("non-SELECT on a read-only connection"),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Run anyway" }),
    ).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "OK" }));
    expect(onCancel).toHaveBeenCalledTimes(1);
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("confirm: Enter activates Run anyway", async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    render(
      <GuardModal
        state={{
          kind: "confirm",
          verdict: { level: "confirm", reasons: ["DELETE without WHERE"] },
        }}
        onConfirm={onConfirm}
        onCancel={onCancel}
      />,
    );
    await user.keyboard("{Enter}");
    expect(onConfirm).toHaveBeenCalledTimes(1);
    expect(onCancel).not.toHaveBeenCalled();
  });

  it("block: Enter dismisses (OK)", async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    render(
      <GuardModal
        state={{
          kind: "block",
          verdict: { level: "block", reasons: ["read-only connection"] },
        }}
        onConfirm={onConfirm}
        onCancel={onCancel}
      />,
    );
    await user.keyboard("{Enter}");
    expect(onCancel).toHaveBeenCalledTimes(1);
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("falls back to a placeholder when there are no reasons", () => {
    render(
      <GuardModal
        state={{ kind: "confirm", verdict: { level: "confirm", reasons: [] } }}
        onConfirm={vi.fn()}
        onCancel={vi.fn()}
      />,
    );
    expect(
      screen.getByText("No specific reason provided."),
    ).toBeInTheDocument();
  });
});
