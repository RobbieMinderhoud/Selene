/**
 * Drives the inline "enter password to connect" prompt.
 *
 * When a connect fails because no password is stored (a `secret` IpcError),
 * {@link connectSession} opens a prompt here and the {@link PasswordPrompt}
 * component renders it. The prompt stays open across a failed attempt (wrong
 * password) so the user can correct it without re-triggering the connect, and
 * resolves the originating connect once the user succeeds or cancels.
 *
 * SECURITY: the typed password lives only in the prompt component's local state
 * and is handed straight to `attempt`; it is never stored here or logged.
 */

import { create } from "zustand";

import type { ConnectionSpec } from "../ipc/types";

export interface PasswordPromptRequest {
  /** The connection being authenticated (for the prompt's title/labels). */
  spec: ConnectionSpec;
  /**
   * Try to connect with `password`. Resolves on success (the prompt closes) and
   * rejects with an `IpcError` on failure (the prompt shows the message and
   * stays open for another try).
   */
  attempt: (password: string) => Promise<void>;
  /** Called when the user dismisses the prompt without connecting. */
  onCancel: () => void;
}

interface PasswordPromptState {
  pending: PasswordPromptRequest | null;
  /** Open a prompt. Any in-flight prompt is cancelled first. */
  open: (request: PasswordPromptRequest) => void;
  /** Close the prompt (does not call `onCancel` — callers decide). */
  close: () => void;
}

export const usePasswordPromptStore = create<PasswordPromptState>(
  (set, get) => ({
    pending: null,
    open: (request) => {
      // Only one prompt at a time — abandon any previous request cleanly so its
      // originating connect doesn't hang forever.
      get().pending?.onCancel();
      set({ pending: request });
    },
    close: () => set({ pending: null }),
  }),
);
