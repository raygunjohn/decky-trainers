// Finding one table among thousands, by filename.
//
// Some repos hold 5606 `.ct` files in a single flat folder, so
// browsing is not a fallback for search here — it is the thing that does not
// work. Ranking has to put the right table in the first few rows of a ~300px
// panel, because every character of the query costs an on-screen keyboard.
//
// Deliberately free of React and `@decky/*` so it can be compiled standalone
// and driven from Node against a captured listing (see CLAUDE.md "Testing").
// It is a sibling of `search.ts`, not a reuse of it: that one folds
// hostile trainer descriptions and walks a feature tree, this one ranks paths.

import type { RepoTable } from "./types";

/** Table names are stable across renders, so folding is memoized by string. */
const cache = new Map<string, string>();
const CACHE_LIMIT = 12000;

/**
 * `Red Dead Redemption 2.CT` -> `red dead redemption 2`, and
 * `0.bountytrainsteamv2_108.ct` -> `0 bountytrainsteamv2 108`.
 *
 * Real filenames in these repos carry version suffixes, uploader tags,
 * underscores and accents; folding them to spaced lowercase ASCII is what
 * makes `dark souls` match `Dark_Souls_III_v1.15.ct`.
 */
export function fold(raw: string): string {
  const hit = cache.get(raw);
  if (hit !== undefined) return hit;

  const folded = raw
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "") // combining marks left by NFKD
    .replace(/\.ct$/i, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, " ")
    .trim();

  if (cache.size >= CACHE_LIMIT) cache.clear();
  cache.set(raw, folded);
  return folded;
}

export interface RankedTable {
  table: RepoTable;
  score: number;
}

export interface RepoSearchResult {
  tables: RepoTable[];
  /** Matches before the cap, so the panel can say what it is not showing. */
  total: number;
}

/**
 * Scores one table against an already-folded query.
 *
 * The name outranks the folder on purpose: a repo that groups tables per game
 * would otherwise return every table in a matching folder ahead of the one
 * actually named for what was typed. Returns 0 for no match.
 */
function score(table: RepoTable, query: string, terms: string[]): number {
  const name = fold(table.name);
  const dir = fold(table.dir);

  let base = 0;
  if (name === query) base = 1000;
  else if (name.startsWith(query)) base = 700;
  else if (name.includes(query)) base = 500;
  else if (terms.every((term) => name.includes(term))) base = 350;
  else if (terms.every((term) => name.includes(term) || dir.includes(term))) base = 150;
  else return 0;

  // Among equally good matches, prefer the shorter name and the shallower
  // path: `Kenshi.CT` is a better answer to "kenshi" than
  // `old/kenshi_x64_beta3_uploaded_by_someone.ct`.
  const brevity = Math.max(0, 60 - name.length);
  const depth = table.dir ? table.dir.split("/").length : 0;
  return base + brevity - depth * 4;
}

/**
 * The tables worth showing for `query`, best first, capped at `limit`.
 *
 * An empty query is browsing, not searching: the backend already sorted the
 * listing by folder then name, so the first `limit` of it is the top of the
 * repo rather than an arbitrary slice.
 */
export function searchTables(
  tables: RepoTable[],
  query: string,
  limit: number,
): RepoSearchResult {
  const folded = fold(query);
  if (!folded) return { tables: tables.slice(0, limit), total: tables.length };

  const terms = folded.split(" ").filter(Boolean);
  const ranked: RankedTable[] = [];
  for (const table of tables) {
    const value = score(table, folded, terms);
    if (value > 0) ranked.push({ table, score: value });
  }

  ranked.sort((a, b) => {
    if (b.score !== a.score) return b.score - a.score;
    // Stable, readable tie-break so the same query always lists the same way.
    return a.table.path.localeCompare(b.table.path);
  });

  return {
    tables: ranked.slice(0, limit).map((entry) => entry.table),
    total: ranked.length,
  };
}
