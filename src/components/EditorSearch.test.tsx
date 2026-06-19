/**
 * EditorSearch: drives the CodeMirror search query API from the themed overlay.
 *
 * Uses a real (headless) EditorView with the `search()` extension so the tests
 * exercise the actual query/match/replace machinery rather than a mock — match
 * counting, the regex toggle (incl. invalid patterns), case sensitivity, and
 * replace-all all run against genuine `@codemirror/search` behaviour.
 */

import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { search } from "@codemirror/search";

import { EditorSearch } from "./EditorSearch";
import { useSettingsStore } from "../state/settingsStore";

function makeView(doc: string): EditorView {
  const parent = document.createElement("div");
  document.body.appendChild(parent);
  return new EditorView({
    state: EditorState.create({ doc, extensions: [search()] }),
    parent,
  });
}

let view: EditorView | null = null;

beforeEach(() => {
  useSettingsStore.getState().resetSettings();
});

afterEach(() => {
  view?.destroy();
  view = null;
});

describe("EditorSearch", () => {
  it("renders nothing while closed", () => {
    view = makeView("select 1");
    const { container } = render(
      <EditorSearch
        view={view}
        open={false}
        replaceMode={false}
        onReplaceModeChange={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it("counts matches case-insensitively, then narrows on Match case", async () => {
    const user = userEvent.setup();
    view = makeView("alpha Alpha ALPHA beta");
    render(
      <EditorSearch
        view={view}
        open={true}
        replaceMode={false}
        onReplaceModeChange={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    await user.type(screen.getByPlaceholderText("Find"), "alpha");
    expect(await screen.findByText("3 found")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Match case" }));
    expect(await screen.findByText("1 found")).toBeInTheDocument();
  });

  it("advances the current-match counter on Next", async () => {
    const user = userEvent.setup();
    view = makeView("foo foo");
    render(
      <EditorSearch
        view={view}
        open={true}
        replaceMode={false}
        onReplaceModeChange={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    await user.type(screen.getByPlaceholderText("Find"), "foo");
    expect(await screen.findByText("2 found")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Next match" }));
    expect(await screen.findByText("1 of 2")).toBeInTheDocument();
  });

  it("treats the query as a regular expression when regex is on", async () => {
    const user = userEvent.setup();
    view = makeView("cat cot cut dog");
    render(
      <EditorSearch
        view={view}
        open={true}
        replaceMode={false}
        onReplaceModeChange={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    await user.click(
      screen.getByRole("button", { name: "Use regular expression" }),
    );
    await user.type(screen.getByPlaceholderText("Find"), "c.t");
    expect(await screen.findByText("3 found")).toBeInTheDocument();
  });

  it("flags an invalid regular expression", async () => {
    const user = userEvent.setup();
    view = makeView("anything");
    render(
      <EditorSearch
        view={view}
        open={true}
        replaceMode={false}
        onReplaceModeChange={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    await user.click(
      screen.getByRole("button", { name: "Use regular expression" }),
    );
    await user.type(screen.getByPlaceholderText("Find"), "c(");
    expect(await screen.findByText("Bad pattern")).toBeInTheDocument();
  });

  it("replaces all matches in the document", async () => {
    const user = userEvent.setup();
    view = makeView("x x x");
    render(
      <EditorSearch
        view={view}
        open={true}
        replaceMode={true}
        onReplaceModeChange={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    await user.type(screen.getByPlaceholderText("Find"), "x");
    await user.type(screen.getByPlaceholderText("Replace"), "y");
    await user.click(screen.getByRole("button", { name: "Replace all" }));

    expect(view.state.doc.toString()).toBe("y y y");
  });

  it("persists toggle state to the settings store", async () => {
    const user = userEvent.setup();
    view = makeView("data");
    render(
      <EditorSearch
        view={view}
        open={true}
        replaceMode={false}
        onReplaceModeChange={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    await user.click(
      screen.getByRole("button", { name: "Use regular expression" }),
    );
    expect(useSettingsStore.getState().search.regexp).toBe(true);
  });
});
