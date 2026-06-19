/**
 * Open a session, recovering from a missing keychain password by prompting the
 * user inline instead of dead-ending.
 *
 * The happy path is a plain `sessionConnect` using the stored secret. When that
 * rejects with `kind: "secret"` (no password stored — e.g. the keychain item
 * was deleted, or the connection was imported), we open the password prompt and
 * retry with what the user types. A successful retry persists the password to
 * the keychain (handled backend-side), so it's silent next time. A wrong
 * password keeps the prompt open with the error; cancelling resolves to `null`.
 *
 * Used by every connect entry point (sidebar click, tab binding) so the
 * recovery is uniform.
 */

import { sessionConnect } from "../ipc/commands";
import { asIpcError } from "../ipc/types";
import type { ConnectionSpec, SessionInfo } from "../ipc/types";
import { usePasswordPromptStore } from "../state/passwordPromptStore";

/**
 * Connect to `connectionId`, prompting for a password if none is stored.
 *
 * Returns the {@link SessionInfo} on success, or `null` if the user cancelled
 * the password prompt. Any non-password error (and the cancelled-prompt case
 * for callers that pass no `spec`) propagates to the caller to surface.
 *
 * `spec` is required to recover from a missing password (it titles the prompt);
 * without it a `secret` error simply propagates.
 */
export async function connectSession(
  connectionId: string,
  spec: ConnectionSpec | undefined,
): Promise<SessionInfo | null> {
  try {
    return await sessionConnect(connectionId);
  } catch (err) {
    if (asIpcError(err).kind !== "secret" || !spec) throw err;
  }

  // No stored password — prompt and retry. The promise resolves when the user
  // either connects successfully or cancels.
  return new Promise<SessionInfo | null>((resolve) => {
    usePasswordPromptStore.getState().open({
      spec,
      attempt: async (password) => {
        // Rejection (wrong password) propagates back to the prompt, which shows
        // the message and stays open. Success resolves the outer connect.
        const info = await sessionConnect(connectionId, password);
        resolve(info);
      },
      onCancel: () => resolve(null),
    });
  });
}
