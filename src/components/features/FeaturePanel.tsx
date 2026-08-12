import { Fragment, useCallback, useMemo, useState } from "react";
import { PanelSection, PanelSectionRow, TextField, ToggleField } from "@decky/ui";

import { AttachPanel } from "./AttachPanel";
import { ExpandRow } from "./ExpandRow";
import { FeatureRow } from "./FeatureRow";
import { breadcrumb, cleanDescription, indexFeatures, searchFeatures } from "../../api/search";
import { flatten, pruneUnresolved, revealedByToggle } from "../../api/tree";
import type { Feature, ProcessBlock } from "../../api/types";

/** A broad query can match hundreds of rows; the QAM only needs the best ones. */
const MAX_RESULTS = 60;
/** The panel is ~300px wide — past this, indentation eats the labels. */
const MAX_INDENT = 3;

export interface FeaturePanelProps {
  features: Feature[];
  process: ProcessBlock | null;
  gameExe?: string;
  /** Process name recorded when this trainer was generated, for AttachPanel. */
  attachHint?: string;
  /** The trainer's reply to the last attach — `attached`, `waiting`, or null. */
  attachStatus?: string | null;
  loading: boolean;
  error: string | null;
  hideUnresolved: boolean;
  onHideUnresolvedChange: (hide: boolean) => void;
  onToggle: (feature: Feature, active: boolean) => void;
  onSetValue: (feature: Feature, value: string) => void;
  onAttach: (processName: string) => void;
}

export function FeaturePanel({
  features,
  process,
  gameExe,
  attachHint,
  attachStatus,
  loading,
  error,
  hideUnresolved,
  onHideUnresolvedChange,
  onToggle,
  onSetValue,
  onAttach,
}: FeaturePanelProps) {
  const [query, setQuery] = useState("");
  // Expansion is keyed by feature id, so it survives the 2s poll replacing the
  // tree. It resets naturally: the panel unmounts when the trainer goes away.
  const [expanded, setExpanded] = useState<Set<number>>(new Set());

  const toggleExpanded = useCallback((id: number) => {
    setExpanded((current) => {
      const next = new Set(current);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  }, []);

  const { tree, hidden } = useMemo(
    () => (hideUnresolved ? pruneUnresolved(features) : { tree: features, hidden: 0 }),
    [features, hideUnresolved],
  );

  const index = useMemo(() => indexFeatures(tree), [tree]);
  const result = useMemo(() => searchFeatures(index, query, MAX_RESULTS), [index, query]);
  const rows = useMemo(() => flatten(tree, expanded), [tree, expanded]);

  const searching = query.trim().length > 0;

  return (
    <>
      <PanelSection title="Trainer">
        <AttachPanel
          process={process}
          gameExe={gameExe}
          attachHint={attachHint}
          attachStatus={attachStatus}
          onAttach={onAttach}
        />
        <PanelSectionRow>
          <TextField
            label="Search features"
            description={
              searching
                ? `${result.total} match${result.total === 1 ? "" : "es"}${
                    result.total > MAX_RESULTS ? ` — showing the top ${MAX_RESULTS}` : ""
                  }`
                : `${index.length} features — type a name, section or value type`
            }
            value={query}
            bShowClearAction
            onChange={(e) => setQuery(e.target.value)}
          />
        </PanelSectionRow>
        <PanelSectionRow>
          <ToggleField
            label="Hide unresolved"
            description={hidden ? `${hidden} rows hidden` : "rows the trainer can't read"}
            bottomSeparator="none"
            checked={hideUnresolved}
            onChange={onHideUnresolvedChange}
          />
        </PanelSectionRow>
        {error && (
          <PanelSectionRow>
            <div style={{ padding: "4px 0", fontSize: "0.8em", color: "#ff6b6b" }}>{error}</div>
          </PanelSectionRow>
        )}
      </PanelSection>

      {loading && features.length === 0 && (
        <PanelSection>
          <PanelSectionRow>
            <div style={{ padding: "4px 0", fontSize: "0.8em" }}>Loading features…</div>
          </PanelSectionRow>
        </PanelSection>
      )}

      {!loading && features.length === 0 && (
        <PanelSection>
          <PanelSectionRow>
            <div style={{ padding: "4px 0", fontSize: "0.8em" }}>
              This trainer exposes no features.
            </div>
          </PanelSectionRow>
        </PanelSection>
      )}

      {/* Search ignores the fold: results show wherever they live in the tree. */}
      {searching && (
        <PanelSection title="Results">
          {result.total === 0 && features.length > 0 && (
            <PanelSectionRow>
              <div style={{ padding: "4px 0", fontSize: "0.8em" }}>
                Nothing matches “{query}”.
              </div>
            </PanelSectionRow>
          )}
          {result.rows.map((row) => (
            <FeatureRow
              key={row.feature.id}
              feature={row.feature}
              indentLevel={0}
              breadcrumb={breadcrumb(row.path)}
              // Results are flat, so a match's own children are never on screen.
              hiddenChildren={row.feature.children?.length ?? 0}
              onToggle={onToggle}
              onSetValue={onSetValue}
            />
          ))}
        </PanelSection>
      )}

      {!searching && features.length > 0 && (
        <PanelSection title="Features">
          {rows.map(({ feature, depth }) => {
            const indent = Math.min(depth, MAX_INDENT);
            const childCount = feature.children?.length ?? 0;

            if (feature.isGroup) {
              return (
                <ExpandRow
                  key={feature.id}
                  title={cleanDescription(feature.description)}
                  count={childCount}
                  indentLevel={indent}
                  expanded={expanded.has(feature.id)}
                  onClick={() => toggleExpanded(feature.id)}
                />
              );
            }

            const byToggle = revealedByToggle(feature);
            return (
              <Fragment key={feature.id}>
                <FeatureRow
                  feature={feature}
                  indentLevel={indent}
                  // Toggle rows hide their children until they're on, so say so.
                  // Everything else gets a visible expander below instead.
                  hiddenChildren={byToggle && !feature.active ? childCount : 0}
                  onToggle={onToggle}
                  onSetValue={onSetValue}
                />
                {childCount > 0 && !byToggle && (
                  <ExpandRow
                    title="Sub-options"
                    count={childCount}
                    indentLevel={Math.min(depth + 1, MAX_INDENT)}
                    expanded={expanded.has(feature.id)}
                    onClick={() => toggleExpanded(feature.id)}
                  />
                )}
              </Fragment>
            );
          })}
        </PanelSection>
      )}
    </>
  );
}
