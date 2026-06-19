import { QueryClient } from "@tanstack/react-query";

/**
 * The single app-wide QueryClient. Exported (rather than created inline in
 * `main.tsx`) so non-React code — notably the file-sync reconciler in
 * `fsSync.ts` — can invalidate the lazy directory listings when a folder
 * changes on disk.
 *
 * Schema/dir reads are cached but go stale quickly, so a structural change (new
 * table, new `.sql` file) shows up on the next expand/refetch.
 */
export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 30_000,
      retry: 1,
      refetchOnWindowFocus: false,
    },
  },
});
