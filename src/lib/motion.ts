/**
 * Motion primitives shared across the UI.
 *
 * Most animation in Selene is pure CSS (transitions + keyframes driven by the
 * `--dur-*` / `--ease-*` tokens in `tokens.css`). The one thing CSS can't do on
 * its own in React is animate an element *out* before it unmounts — once the
 * component returns `null`, the node is gone and there's nothing left to
 * transition. `usePresence` bridges that: it keeps a closing element mounted for
 * the length of its exit animation, exposing a `data-state` the CSS keys off.
 *
 * Durations here mirror the `--dur-*` CSS tokens (ms). They MUST stay in sync:
 * the JS timer below decides when to actually unmount, and the CSS decides how
 * long the exit visually takes — a mismatch either clips the animation or leaves
 * a dead node on screen.
 */

import { useEffect, useState } from "react";

/** Duration scale in milliseconds — mirrors the `--dur-*` tokens. */
export const MOTION = {
  micro: 90,
  fast: 150,
  base: 220,
  slow: 360,
} as const;

/**
 * Whether the user (or OS) asked for reduced motion. Guarded so it is safe in
 * non-browser/test environments where `matchMedia` is absent (jsdom): we treat
 * "unknown" as "motion allowed", since the global CSS guard is the real
 * enforcement point and this only shortens JS unmount timers.
 */
export function prefersReducedMotion(): boolean {
  if (
    typeof window === "undefined" ||
    typeof window.matchMedia !== "function"
  ) {
    return false;
  }
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

export type PresenceState = "open" | "closed";

export interface Presence {
  /** Whether the element should be in the DOM at all. */
  mounted: boolean;
  /**
   * Drives the exit animation. Put it on the animated node as
   * `data-state={state}` and define `[data-state="closed"]` keyframes in CSS.
   */
  state: PresenceState;
}

/**
 * Keep an element mounted across its exit animation.
 *
 * When `open` flips to `false`, the element stays mounted (with
 * `state === "closed"`) for `duration` ms so its exit animation can play, then
 * unmounts. Under reduced motion the exit is instant. The enter animation is
 * left to CSS: a freshly-mounted node with `data-state="open"` will run its
 * entrance keyframes automatically.
 *
 * @param open      Whether the element should be visible.
 * @param duration  Exit animation length in ms (default {@link MOTION.base});
 *                  must match the CSS exit-animation duration.
 */
export function usePresence(
  open: boolean,
  duration: number = MOTION.base,
): Presence {
  const [mounted, setMounted] = useState(open);

  useEffect(() => {
    if (open) {
      setMounted(true);
      return;
    }
    // Closing: hold the node for the exit animation, then drop it. Reduced
    // motion skips the wait entirely.
    if (prefersReducedMotion()) {
      setMounted(false);
      return;
    }
    const timer = setTimeout(() => setMounted(false), duration);
    return () => clearTimeout(timer);
  }, [open, duration]);

  return { mounted, state: open ? "open" : "closed" };
}
