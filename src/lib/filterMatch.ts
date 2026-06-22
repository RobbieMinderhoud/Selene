/**
 * Shared name-matching predicate for the sidebar's type-to-filter (connections,
 * files, schema tree). Case-insensitive substring match; a blank query matches
 * everything. Single source of truth so every panel filters identically.
 */
export function matches(text: string, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (q === "") return true;
  return text.toLowerCase().includes(q);
}
