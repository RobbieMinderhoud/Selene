/**
 * Floating chip that shows the active type-to-filter query for a sidebar panel.
 * Mounted only while a filter is active; animates in/out (reduced-motion safe
 * via usePresence). Render it inside a `position: relative` panel wrapper.
 */

import { MOTION, usePresence } from "../lib/motion";
import styles from "../lib/useTypeToFilter.module.css";
import { SearchIcon } from "./icons";

export function FilterIndicator({ query }: { query: string }) {
  const { mounted, state } = usePresence(query.length > 0, MOTION.fast);
  if (!mounted) return null;
  return (
    <div className={styles.indicator} data-state={state} aria-live="polite">
      <span className={styles.icon} aria-hidden>
        <SearchIcon />
      </span>
      <span className={styles.text}>{query}</span>
    </div>
  );
}
