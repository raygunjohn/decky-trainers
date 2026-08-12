// Feature search. Trainer tables carry thousands of rows with cosmetic noise in
// the descriptions — `}]}{][{}{}{}Main CodeBase}]}{][{}{}{}`, stylized Unicode
// like `𝓣ʀᴇᴀꜱᴜʀᴇ 𝓕ɪɴᴅᴇʀ` — so a naive `includes()` misses almost everything a
// user actually types. Everything here matches on a normalized ASCII form.

import type { Feature } from "./types";

/** Small-capital letters: NFKD leaves these alone, so they need a table. */
const SMALL_CAPS: Record<string, string> = {
  "ᴀ": "a", "ʙ": "b", "ᴄ": "c", "ᴅ": "d", "ᴇ": "e",
  "ꜰ": "f", "ɢ": "g", "ʜ": "h", "ɪ": "i", "ᴊ": "j",
  "ᴋ": "k", "ʟ": "l", "ᴍ": "m", "ɴ": "n", "ᴏ": "o",
  "ᴘ": "p", "ʀ": "r", "ꜱ": "s", "ᴛ": "t", "ᴜ": "u",
  "ᴠ": "v", "ᴡ": "w", "ʏ": "y", "ᴢ": "z",
};

// Descriptions are stable across polls, so normalization is memoized by raw
// string rather than recomputed for every row on every 2s refresh.
const cache = new Map<string, string>();
const CACHE_LIMIT = 8000;

/**
 * Folds a description to lowercase ASCII words: `𝓣ʀᴇᴀꜱᴜʀᴇ 𝓕ɪɴᴅᴇʀ` -> `treasure
 * finder`, `{}{}{}{[{{BASEWaypoint][][][{}{}{}` -> `basewaypoint`.
 */
export function normalize(raw: string): string {
  const hit = cache.get(raw);
  if (hit !== undefined) return hit;

  const folded = raw
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "") // combining marks left by NFKD
    .replace(/[^\x00-\x7f]/g, (ch) => SMALL_CAPS[ch] ?? " ")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, " ")
    .trim();

  if (cache.size >= CACHE_LIMIT) cache.clear();
  cache.set(raw, folded);
  return folded;
}

/** Strips the decorative `{}[]` runs trainers wrap section titles in. */
export function cleanDescription(raw: string): string {
  const stripped = raw.replace(/^[\s{}[\]]+/, "").replace(/[\s{}[\]]+$/, "");
  return stripped || raw;
}

export interface IndexedFeature {
  feature: Feature;
  /** Pre-order position, so ranked results can be put back in table order. */
  order: number;
  /** Raw descriptions of every ancestor, outermost first — `length` is depth. */
  path: string[];
  /** Normalized own description, used for ranking. */
  own: string;
  /** Own description + every ancestor + type + id, used for matching. */
  haystack: string;
}

/**
 * Walks the tree into a flat list of searchable rows. Group nodes aren't rows
 * of their own — their text lives on in the haystack of everything beneath
 * them, so a query hitting a group pulls up its whole subtree. Rows that carry
 * children are both: a row, and an ancestor of the rows below.
 */
export function indexFeatures(features: Feature[]): IndexedFeature[] {
  const rows: IndexedFeature[] = [];

  const walk = (nodes: Feature[], path: string[], ancestors: string) => {
    for (const feature of nodes) {
      const own = normalize(feature.description);
      if (!feature.isGroup) {
        rows.push({
          feature,
          order: rows.length,
          path,
          own,
          haystack: `${own} ${ancestors} ${normalize(feature.type)} ${feature.id}`,
        });
      }
      if (feature.children?.length) {
        const nested = `${ancestors} ${own}`.trim();
        walk(feature.children, [...path, feature.description], nested);
      }
    }
  };

  walk(features, [], "");
  return rows;
}

/**
 * Renders an ancestor path for display, trimming from the outside in so the
 * nearest — most identifying — ancestor always survives.
 */
export function breadcrumb(path: string[], maxChars = 44): string {
  const parts = path.map(cleanDescription).filter(Boolean);
  if (parts.length === 0) return "";

  let text = parts.join(" › ");
  while (parts.length > 1 && text.length > maxChars) {
    parts.shift();
    text = `… › ${parts.join(" › ")}`;
  }
  return text;
}

/** 0 = absent. Whole-string beats prefix beats word-start beats mid-word. */
function termScore(hay: string, term: string): number {
  const at = hay.indexOf(term);
  if (at < 0) return 0;
  if (at === 0) return hay.length === term.length ? 8 : 4;
  return hay[at - 1] === " " ? 2 : 1;
}

export interface SearchResult {
  /** Best matches first, capped at `limit`. */
  rows: IndexedFeature[];
  /** How many rows matched before the cap. */
  total: number;
}

/**
 * Ranked search over an index. A row matches when every term appears somewhere
 * in its haystack, so `chest bards` finds the "Bards Crossing" row under the
 * "Chest Locations" group, and a term hitting only the group title pulls in
 * that whole group. Rows matching in their own description rank above those.
 */
export function searchFeatures(
  index: IndexedFeature[],
  query: string,
  limit: number,
): SearchResult {
  const terms = normalize(query).split(" ").filter(Boolean);
  // No query: the whole table, in table order, uncapped — the cap only exists
  // to keep a broad search from rendering thousands of rows.
  if (terms.length === 0) return { rows: index, total: index.length };

  const scored: { row: IndexedFeature; score: number }[] = [];
  for (const row of index) {
    let score = 0;
    let matched = true;
    for (const term of terms) {
      if (termScore(row.haystack, term) === 0) {
        matched = false;
        break;
      }
      score += termScore(row.own, term);
    }
    if (matched) scored.push({ row, score });
  }

  scored.sort((a, b) => b.score - a.score || a.row.order - b.row.order);
  return { rows: scored.slice(0, limit).map((s) => s.row), total: scored.length };
}
