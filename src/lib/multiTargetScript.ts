/**
 * Build the "generate script" output for the multi-target view: one block per
 * database that switches into it, announces itself, runs the query, and prints a
 * separator. Mirrors the original PHP generator. Kept pure (no React/CodeMirror
 * imports) so it is cheap to unit-test and reuse.
 */
export function buildScriptText(databases: string[], query: string): string {
  const sep = "-".repeat(32);
  const body = query.trim();
  return databases
    .map(
      (db) =>
        `USE [${db}]\n` +
        `RAISERROR(N'Running script in database: [${db}]', 10, 1) WITH NOWAIT\n` +
        `${body}\n` +
        `PRINT 'Query executed for ${db}'\n` +
        `${sep}\n\n`,
    )
    .join("");
}
