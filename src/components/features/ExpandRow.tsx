import { ButtonItem, PanelSectionRow } from "@decky/ui";

export interface ExpandRowProps {
  title: string;
  /** Number of direct children; 0 renders a bare, unclickable header. */
  count: number;
  indentLevel: number;
  expanded: boolean;
  onClick: () => void;
}

/**
 * A collapsed branch of the tree. Used for group nodes, and for value rows
 * whose own click target is taken by the keyboard modal.
 */
export function ExpandRow({ title, count, indentLevel, expanded, onClick }: ExpandRowProps) {
  return (
    <PanelSectionRow>
      <ButtonItem
        layout="below"
        indentLevel={indentLevel}
        bottomSeparator="none"
        // Childless headers exist to carry instructions ("Save Coords") — they
        // aren't broken sections, so they say nothing rather than "empty".
        description={count ? `${count} item${count === 1 ? "" : "s"}` : undefined}
        disabled={count === 0}
        onClick={onClick}
      >
        <span style={{ fontWeight: "bold" }}>
          {count > 0 && (expanded ? "▾ " : "▸ ")}
          {title}
        </span>
      </ButtonItem>
    </PanelSectionRow>
  );
}
