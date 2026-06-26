/**
 * Host-platform detection for the frontend.
 *
 * Single source of truth so chrome that differs per OS (the macOS native menu
 * vs. the Windows in-app title bar / settings gear / window controls) all read
 * the same flags. `navigator.platform` is deprecated but still the most reliable
 * signal inside WebView2 / WKWebView; we fall back to the user-agent string.
 *
 * Computed once at module load — the platform never changes within a session.
 */

const haystack = navigator.platform || navigator.userAgent;

export const isMac = /mac/i.test(haystack);
export const isWindows = /win/i.test(haystack);
