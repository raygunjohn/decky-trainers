// Shaping the feature tree for display: what's readable, what's unfolded.

import { isUnresolved } from "./types";
import type { Feature } from "./types";

/** A row the trainer can't read out of the game yet — pure `??` noise. */
export function unreadable(feature: Feature): boolean {
  return Boolean(feature.actions?.setValue) && isUnresolved(feature.value);
}

/**
 * Which widget a row renders as. Single source of truth: `FeatureRow` picks its
 * branch from this, and the fold below depends on which one it picked.
 */
export type RowKind = "value" | "toggle" | "static";

export function rowKind(feature: Feature): RowKind {
  if (feature.actions?.setValue) return "value";
  if (feature.actions?.toggle) return "toggle";
  return "static";
}

/**
 * Rows rendered as a plain switch reveal their children when the cheat is
 * activated. Everything else — groups, and value rows whose click is already
 * taken by the keyboard modal — needs an explicit expander instead.
 */
export function revealedByToggle(feature: Feature): boolean {
  return !feature.isGroup && rowKind(feature) === "toggle";
}

/**
 * Whether the table asks for this record's children to stay out of sight.
 *
 * The visibility rule is:
 *
 * ```text
 * collapse when (inactive and hideChildren) or alwaysHideChildren,
 * otherwise expand
 * ```
 *
 * `alwaysHideChildren` is unconditional; `hideChildren` only hides while the
 * record is off. Tables use these for their plumbing — rdr2.CT hides the whole
 * "BASE" layer this way, which its startup cascade legitimately turns on. Those
 * records are active regardless; you simply never see them. A panel that lists
 * them reads as "everything got enabled behind my back", which is how this was
 * noticed.
 *
 * Hiding, not pruning: the rows still exist, still poll, and still show up in
 * search — this only decides whether a branch unfolds by default.
 */
export function childrenHiddenByTable(feature: Feature): boolean {
  if (feature.alwaysHideChildren) return true;
  return Boolean(feature.hideChildren) && !feature.active;
}

export interface PrunedTree {
  tree: Feature[];
  hidden: number;
}

/** Prunes unreadable leaves, keeping any branch that still leads somewhere. */
export function pruneUnresolved(nodes: Feature[]): PrunedTree {
  const tree: Feature[] = [];
  let hidden = 0;

  for (const node of nodes) {
    const pruned = node.children?.length ? pruneUnresolved(node.children) : null;
    if (pruned) hidden += pruned.hidden;
    const children = pruned?.tree;
    const hasChildren = Boolean(children && children.length > 0);

    // A group that lost every child has nothing left to head.
    if (node.isGroup && node.children?.length && !hasChildren) continue;
    if (!node.isGroup && unreadable(node) && !hasChildren) {
      hidden += 1;
      continue;
    }
    tree.push(children ? { ...node, children } : node);
  }

  return { tree, hidden };
}

export interface VisibleRow {
  feature: Feature;
  depth: number;
}

/** The tree as it's currently unfolded: a closed branch contributes one row. */
export function flatten(
  nodes: Feature[],
  expanded: Set<number>,
  depth = 0,
  out: VisibleRow[] = [],
): VisibleRow[] {
  for (const feature of nodes) {
    out.push({ feature, depth });
    if (!feature.children?.length) continue;
    // An explicit tap always wins: `expanded` is the user asking to look
    // inside, and refusing that would make a branch permanently unopenable.
    // Otherwise the table's own wishes decide, then the toggle rule.
    const open = expanded.has(feature.id)
      ? true
      : childrenHiddenByTable(feature)
        ? false
        : revealedByToggle(feature)
          ? feature.active
          : false;
    if (open) flatten(feature.children, expanded, depth + 1, out);
  }
  return out;
}
