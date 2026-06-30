/**
 * Helpers for manipulating **server-side** filesystem paths (the SQL Server
 * host, which may be Windows or Linux). The separator is inferred from the path
 * itself — `\` if it contains a backslash, otherwise `/` — so we honour the
 * server's OS without knowing it up front.
 */

/** The separator `p` uses: `\` if it contains one, else `/`. */
export function sep(p: string): string {
  return p.includes("\\") ? "\\" : "/";
}

/** Join a directory and a child name with the directory's separator. */
export function joinServerPath(dir: string, name: string): string {
  if (!dir) return name;
  const s = sep(dir);
  return dir.endsWith(s) ? `${dir}${name}` : `${dir}${s}${name}`;
}

/** The parent directory of `p`, keeping the root. */
export function parentPath(p: string): string {
  const s = sep(p);
  const trimmed = p.replace(/[\\/]+$/, "");
  const i = trimmed.lastIndexOf(s);
  if (i < 0) return p;
  return trimmed.slice(0, i + 1) || s;
}

/** The directory portion of a full path (everything before the last separator). */
export function dirName(p: string): string {
  const s = sep(p);
  const i = p.replace(/[\\/]+$/, "").lastIndexOf(s);
  return i <= 0 ? p.slice(0, i + 1) || s : p.slice(0, i);
}

/** The final component of a path. */
export function baseName(p: string): string {
  const trimmed = p.replace(/[\\/]+$/, "");
  const i = trimmed.lastIndexOf(sep(trimmed));
  return i < 0 ? trimmed : trimmed.slice(i + 1);
}
