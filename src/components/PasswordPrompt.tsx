/**
 * Inline "enter password to connect" prompt.
 *
 * Shown when a connect needs a password that isn't in the keychain (deleted, or
 * an imported connection). Driven by {@link usePasswordPromptStore}: it calls
 * the pending request's `attempt`, staying open with an inline error if the
 * password is wrong, and closing once the connect succeeds or is cancelled.
 *
 * SECURITY: the typed password lives only in this component's local state, is
 * handed straight to `attempt`, and is cleared as soon as the prompt closes.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import { asIpcError } from "../ipc/types";
import { usePasswordPromptStore } from "../state/passwordPromptStore";
import { Modal } from "./Modal";
import styles from "./PasswordPrompt.module.css";

export function PasswordPrompt() {
  const pending = usePasswordPromptStore((s) => s.pending);
  const close = usePasswordPromptStore((s) => s.close);

  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // Reset form state each time a new prompt opens, and focus the field.
  const specId = pending?.spec.id ?? null;
  useEffect(() => {
    if (specId === null) return;
    setPassword("");
    setError(null);
    setBusy(false);
    // Focus after the modal's open animation has begun.
    const t = window.setTimeout(() => inputRef.current?.focus(), 0);
    return () => window.clearTimeout(t);
  }, [specId]);

  // Stable identity: this is `Modal`'s `onClose`, and Modal re-runs its
  // focus effect whenever `onClose` changes. An inline closure would change on
  // every keystroke and keep stealing focus back to the dialog card.
  const cancel = useCallback(() => {
    if (busy) return;
    pending?.onCancel();
    setPassword("");
    close();
  }, [busy, pending, close]);

  async function submit() {
    if (!pending || busy || !password) return;
    setBusy(true);
    setError(null);
    try {
      await pending.attempt(password);
      // Success: the originating connect has resolved. Drop the password and close.
      setPassword("");
      close();
    } catch (e) {
      setError(asIpcError(e).message);
      setBusy(false);
      inputRef.current?.select();
    }
  }

  const footer = (
    <>
      <button type="button" onClick={cancel} disabled={busy}>
        Cancel
      </button>
      <button
        type="button"
        className="primary"
        onClick={submit}
        disabled={busy || !password}
      >
        {busy ? "Connecting…" : "Connect"}
      </button>
    </>
  );

  return (
    <Modal
      open={pending !== null}
      title="Password required"
      onClose={cancel}
      footer={footer}
      width={420}
    >
      <form
        className={styles.form}
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <p className={styles.intro}>
          Enter the password for <strong>{pending?.spec.name}</strong>
          {pending?.spec.auth.username ? (
            <>
              {" "}
              (<span className={styles.user}>{pending.spec.auth.username}</span>
              )
            </>
          ) : null}
          .
        </p>
        <div className={styles.field}>
          <label htmlFor="prompt-pass">Password</label>
          <input
            id="prompt-pass"
            ref={inputRef}
            type="password"
            value={password}
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            disabled={busy}
            onChange={(e) => setPassword(e.target.value)}
          />
        </div>
        {error && (
          <p className={styles.error} role="alert">
            {error}
          </p>
        )}
        {/* Submit on Enter without a visible default button. */}
        <button type="submit" className="visually-hidden" tabIndex={-1}>
          Submit password
        </button>
      </form>
    </Modal>
  );
}
