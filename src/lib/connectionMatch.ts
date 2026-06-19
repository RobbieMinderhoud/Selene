/**
 * Match an opened file to a saved connection by name.
 *
 * Selene auto-connects a freshly opened `.sql` file to the connection whose name
 * appears in the file name — e.g. opening `pr02db02b_shared_01.sql` connects to a
 * connection named `pr02db02b`. The match is:
 *
 *  - **case-insensitive** — `PR02DB02B_x.sql` matches a `pr02db02b` connection;
 *  - **token-bounded** — the connection name must sit on non-alphanumeric
 *    boundaries (or a string edge), so `pr02db02b` matches inside
 *    `pr02db02b_shared` but a shorter, *different* server `pr02db02` does **not**
 *    (the following `b` is alphanumeric, so that occurrence isn't a real token).
 *    This keeps a short connection name from matching as an accidental substring
 *    of a longer, unrelated identifier;
 *  - **longest-wins** — when several connection names match as tokens, the most
 *    specific (longest) name is chosen; ties keep the first in list order.
 *
 * Pure (no IPC, no store) so it is exhaustively unit-testable.
 */

/** The minimal connection shape the matcher needs. */
export interface NamedConnection {
  id: string;
  name: string;
}

/** ASCII-alphanumeric test — the alphabet our token boundaries care about. */
function isAlnum(ch: string): boolean {
  return /[a-z0-9]/i.test(ch);
}

/**
 * True if `needle` occurs in `haystack` flanked by non-alphanumeric characters
 * (or the string edge) on both sides — i.e. as a standalone token rather than a
 * substring of a longer alphanumeric run. Both arguments must already be the
 * same case.
 */
function occursAsToken(haystack: string, needle: string): boolean {
  for (let from = 0; ; ) {
    const at = haystack.indexOf(needle, from);
    if (at === -1) return false;
    const before = at === 0 ? "" : haystack[at - 1];
    const after = haystack[at + needle.length] ?? "";
    if (
      (before === "" || !isAlnum(before)) &&
      (after === "" || !isAlnum(after))
    ) {
      return true;
    }
    from = at + 1;
  }
}

/**
 * Return the `id` of the connection whose name appears as a token in `fileName`,
 * or `null` when none matches. See the module header for the matching rules.
 */
export function matchConnectionForFile(
  fileName: string,
  connections: readonly NamedConnection[],
): string | null {
  const haystack = fileName.toLowerCase();
  let best: { id: string; len: number } | null = null;
  for (const conn of connections) {
    const needle = conn.name.trim().toLowerCase();
    if (!needle) continue; // a connection with a blank name can't match
    if (!occursAsToken(haystack, needle)) continue;
    if (!best || needle.length > best.len) {
      best = { id: conn.id, len: needle.length };
    }
  }
  return best?.id ?? null;
}
