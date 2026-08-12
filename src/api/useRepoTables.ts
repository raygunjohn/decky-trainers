import { useCallback, useEffect, useRef, useState } from "react";

import { listRepoTables } from "./backend";
import type { RepoListing, RepoTable } from "./types";

export interface UseRepoTables {
  tables: RepoTable[];
  /** `owner/repo`, for a status line that fits in ~300px. */
  label: string;
  ref: string;
  commit: string;
  truncated: boolean;
  /** Set when the listing on screen is older than a failed refresh. */
  stale: boolean;
  /** Why it is stale, or another non-fatal note. Not an error — the list works. */
  warning: string | null;
  loading: boolean;
  error: string | null;
  /** The URL the backend has stored, restored on mount. */
  repoUrl: string;
  load: (url: string, refresh?: boolean) => Promise<void>;
  refresh: () => Promise<void>;
}

const EMPTY: RepoTable[] = [];

/**
 * The repo listing, owned in one place the way `useInstalled` owns the local one.
 *
 * It deliberately does **not** fetch on mount with no URL: an empty repo field
 * is the normal state for someone who only ever builds from local tables, and
 * a failing call there would put an error in front of them for a feature they
 * have not asked for.
 */
export function useRepoTables(): UseRepoTables {
  const [listing, setListing] = useState<RepoListing | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // The last URL actually listed, so `refresh` does not need it passed back in
  // and cannot refresh a different repo than the one on screen.
  const current = useRef("");
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const load = useCallback(async (url: string, refresh = false) => {
    setLoading(true);
    setError(null);
    try {
      const result = await listRepoTables(url ?? "", refresh);
      if (!mounted.current) return;
      if (result?.ok) {
        current.current = result.repoUrl || url || "";
        setListing(result);
      } else {
        // Keep whatever is already listed: a failed refresh should not empty
        // the panel someone was about to build from.
        setError(result?.error ?? "the repository could not be listed");
      }
    } catch (e) {
      if (mounted.current) setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (mounted.current) setLoading(false);
    }
  }, []);

  const refresh = useCallback(() => load(current.current, true), [load]);

  // Restore the stored repo on mount. An empty URL is not an error here — the
  // backend answers with `ok: false` and no repo, and the tab shows its
  // "paste a URL" state instead.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const result = await listRepoTables("", false);
        if (cancelled || !mounted.current) return;
        if (result?.ok) {
          current.current = result.repoUrl || "";
          setListing(result);
        } else if (result?.repoUrl) {
          // A stored URL that would not list is worth reporting; a blank one
          // is just someone who has not used this tab.
          current.current = result.repoUrl;
          setListing({ ok: false, tables: [], repoUrl: result.repoUrl });
          setError(result.error ?? null);
        }
      } catch {
        /* the tab renders its empty state */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  return {
    tables: listing?.tables ?? EMPTY,
    label: listing?.label ?? "",
    ref: listing?.ref ?? "",
    commit: listing?.commit ?? "",
    truncated: Boolean(listing?.truncated),
    stale: Boolean(listing?.stale),
    warning: listing?.warning ?? null,
    repoUrl: listing?.repoUrl ?? "",
    loading,
    error,
    load,
    refresh,
  };
}
