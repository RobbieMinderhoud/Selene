/**
 * Tiny path helpers for the webview (no Node `path` here). Paths may use either
 * separator since they originate from canonicalized OS paths across platforms.
 */

/** Final path component (file or directory name). */
export function basename(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

/** Parent directory of a path; returns the path unchanged if it has no parent. */
export function parentDir(path: string): string {
  const idx = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return idx > 0 ? path.slice(0, idx) : path;
}
