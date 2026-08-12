import { ButtonItem, PanelSectionRow, ToggleField, showModal } from "@decky/ui";

import { SetValueModal } from "./SetValueModal";
import { rowKind } from "../../api/tree";
import { isUnresolved } from "../../api/types";
import type { Feature } from "../../api/types";

export interface FeatureRowProps {
  feature: Feature;
  indentLevel: number;
  /** Ancestor path — shown in search results, where rows lose their context. */
  breadcrumb?: string;
  /** Children this row has that aren't on screen right now; 0 to say nothing. */
  hiddenChildren?: number;
  onToggle: (feature: Feature, active: boolean) => void;
  onSetValue: (feature: Feature, value: string) => void;
}

/**
 * Builds the sub-label. In the tree it's the row's type; in search results the
 * breadcrumb earns that space instead — the widget already shows the kind, and
 * the panel is too narrow for both.
 */
function describe(
  feature: Feature,
  breadcrumb: string | undefined,
  hiddenChildren: number | undefined,
  withValue: boolean,
): string {
  const value = withValue && feature.value && !isUnresolved(feature.value) ? feature.value : "";
  const parts: string[] = [];

  if (breadcrumb) {
    parts.push(breadcrumb);
    if (value) parts.push(value);
  } else {
    parts.push(value ? `${feature.type} · ${value}` : feature.type);
  }
  if (hiddenChildren) {
    parts.push(`${hiddenChildren} sub-option${hiddenChildren === 1 ? "" : "s"}`);
  }
  return parts.join(" · ");
}

export function FeatureRow({
  feature,
  indentLevel,
  breadcrumb,
  hiddenChildren,
  onToggle,
  onSetValue,
}: FeatureRowProps) {
  const unresolved = isUnresolved(feature.value);

  switch (rowKind(feature)) {
    // Value rows open the keyboard in a modal; the row itself stays read-only.
    case "value":
      return (
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            indentLevel={indentLevel}
            bottomSeparator="none"
            label={feature.description}
            description={describe(feature, breadcrumb, hiddenChildren, false)}
            disabled={unresolved}
            onClick={() =>
              showModal(
                <SetValueModal
                  title={feature.description}
                  initialValue={unresolved ? "" : (feature.value as string)}
                  onConfirm={(value) => onSetValue(feature, value)}
                />,
              )
            }
          >
            {unresolved ? "n/a" : (feature.value as string)}
          </ButtonItem>
        </PanelSectionRow>
      );

    case "toggle":
      return (
        <PanelSectionRow>
          <ToggleField
            label={feature.description}
            description={describe(feature, breadcrumb, hiddenChildren, true)}
            indentLevel={indentLevel}
            bottomSeparator="none"
            checked={feature.active}
            onChange={(checked) => onToggle(feature, checked)}
          />
        </PanelSectionRow>
      );

    // No advertised actions: display only.
    default:
      return (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", paddingLeft: indentLevel * 12, fontSize: "0.8em" }}>
            <div>
              {feature.description}
              {!unresolved && feature.value ? `: ${feature.value}` : ""}
            </div>
            {breadcrumb && <div style={{ opacity: 0.5 }}>{breadcrumb}</div>}
          </div>
        </PanelSectionRow>
      );
  }
}
