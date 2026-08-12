import { useState } from "react";
import type { CSSProperties } from "react";
import {
  ConfirmModal,
  DialogButton,
  Field,
  Focusable,
  PanelSectionRow,
  showModal,
} from "@decky/ui";
import { FaPlay, FaTrash } from "react-icons/fa";

import { deleteTrainer } from "../../api/backend";
import type { InstalledTrainer } from "../../api/types";

export interface InstalledListProps {
  trainers: InstalledTrainer[];
  loading: boolean;
  /** Launching needs a game to inject into; without one Play is dead weight. */
  canLaunch: boolean;
  trainerDir: string;
  onLaunch: (trainer: InstalledTrainer) => void;
  onDeleted: () => void;
}

/** Square enough to hit with a thumbstick, narrow enough for two side by side. */
const ICON_BUTTON: CSSProperties = {
  minWidth: 0,
  width: "40px",
  padding: "8px 0",
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
};

/**
 * Everything the sidecar knows, in the order it's worth the width: the process
 * name is the most useful of the three — it's what the trainer has to attach to
 * — so it survives ahead of the version. A hand-copied trainer has no sidecar,
 * so its filename takes the slot instead.
 *
 * A trainer built from a downloaded table says which repo it came from, in the
 * version's place: the table is the thing anyone would want to re-find, and
 * these rows almost never carry a version.
 */
function subtitle(trainer: InstalledTrainer): string {
  const parts =
    trainer.source === "generated"
      ? [
          trainer.processName,
          trainer.origin === "github" && trainer.repo
            ? trainer.repo
            : trainer.version && `v${trainer.version}`,
        ]
      : [trainer.exe];
  return [...parts, `${trainer.sizeMB} MB`].filter(Boolean).join(" · ");
}

export function InstalledList({
  trainers,
  loading,
  canLaunch,
  trainerDir,
  onLaunch,
  onDeleted,
}: InstalledListProps) {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const remove = async (trainer: InstalledTrainer) => {
    setBusy(trainer.id);
    setError(null);
    try {
      const result = await deleteTrainer(trainer.id, trainer.path);
      if (result.ok) onDeleted();
      else setError(result.error ?? "Could not delete that trainer.");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };

  const confirmRemove = (trainer: InstalledTrainer) =>
    showModal(
      <ConfirmModal
        strTitle={`Delete ${trainer.gameName || trainer.exe}?`}
        strDescription={`${trainer.exe} (${trainer.sizeMB} MB) will be removed from the Deck. You can build it again from its .CT table.`}
        strOKButtonText="Delete"
        bDestructiveWarning
        onOK={() => remove(trainer)}
      />,
    );

  if (loading) {
    return (
      <PanelSectionRow>
        <div style={{ padding: "8px 0", fontSize: "0.8em" }}>Reading installed trainers…</div>
      </PanelSectionRow>
    );
  }

  if (trainers.length === 0) {
    return (
      <PanelSectionRow>
        <div style={{ padding: "8px 0", fontSize: "0.8em" }}>
          Nothing installed yet. Build one from a <code>.CT</code> table on the{" "}
          <b>Generate</b> tab, or copy a trainer into <b>{trainerDir || "~/trainers"}</b>.
        </div>
      </PanelSectionRow>
    );
  }

  return (
    <>
      {error && (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em", color: "#ff6b6b" }}>{error}</div>
        </PanelSectionRow>
      )}
      {trainers.map((trainer) => (
        <PanelSectionRow key={trainer.id}>
          <Field
            label={trainer.gameName || trainer.exe}
            description={subtitle(trainer)}
            bottomSeparator="none"
            padding="compact"
            childrenLayout="inline"
            childrenContainerWidth="min"
          >
            <Focusable style={{ display: "flex", gap: "6px" }}>
              <DialogButton
                style={ICON_BUTTON}
                disabled={!canLaunch || busy !== null}
                onClick={() => onLaunch(trainer)}
              >
                <FaPlay />
              </DialogButton>
              <DialogButton
                style={ICON_BUTTON}
                disabled={busy !== null}
                onClick={() => confirmRemove(trainer)}
              >
                <FaTrash />
              </DialogButton>
            </Focusable>
          </Field>
        </PanelSectionRow>
      ))}
      {!canLaunch && (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em", opacity: 0.7 }}>
            Start a game to launch one of these.
          </div>
        </PanelSectionRow>
      )}
    </>
  );
}
