import { useEffect, useRef, useState } from "react";
import { ButtonItem, PanelSectionRow, TextField } from "@decky/ui";

import type { ProcessBlock } from "../../api/types";

export interface AttachPanelProps {
  process: ProcessBlock | null;
  /** Exe name the backend captured from the game process, if it has one. */
  gameExe?: string;
  /**
   * `processName` from the trainer's sidecar — a better answer than `gameExe`
   * when a launcher chain (PlayRDR2.exe -> RDR2.exe) fooled the `/proc` scan.
   */
  attachHint?: string;
  /** The trainer's reply to the last attach: `attached`, `waiting`, or null. */
  attachStatus?: string | null;
  onAttach: (processName: string) => void;
}

/**
 * Attaching is always an explicit act: the plugin fills the field in with its
 * best guess and stops there. Nothing is ever POSTed on the user's behalf — a
 * wrong guess binds the trainer to the wrong process and then presents as the
 * trainer being broken, so the name that gets used is the one on screen, that
 * the user saw and accepted.
 *
 * The field therefore stays visible and editable even once attached, so
 * re-attaching (wrong process, game restarted) never means hunting for it.
 */
export function AttachPanel({
  process,
  gameExe,
  attachHint,
  attachStatus,
  onAttach,
}: AttachPanelProps) {
  const suggestion = attachHint || gameExe || "";
  const [name, setName] = useState(suggestion);
  const typed = useRef(false);

  // The suggestion is not known at mount: it arrives with the launch result or
  // the first poll. Capturing it once would leave the field permanently empty.
  useEffect(() => {
    if (!typed.current && suggestion) setName(suggestion);
  }, [suggestion]);

  const attached = Boolean(process?.attached);
  const trimmed = name.trim();
  // Re-attaching to the process it already holds is a no-op worth preventing:
  // it costs a POST and reads as a broken button.
  const redundant = attached && trimmed === (process?.processName ?? "");

  // `waiting` means the trainer took the name and will bind itself the moment
  // that process appears — the game isn't running (or is still loading) yet.
  // Without saying so, a successful attach is indistinguishable from a no-op.
  const waiting = !attached && attachStatus === "waiting";

  let status: string;
  let colour: string | undefined;
  if (attached) {
    status = "";
  } else if (waiting) {
    status = `Waiting for ${name || "that process"} — the trainer will attach as soon as the game is running.`;
    colour = "#8ab4f8";
  } else if (process?.canSelfAttach) {
    status = "Not attached yet — the table attaches itself, this usually clears in a few seconds.";
    colour = "#ffcc66";
  } else {
    status = "Not attached. Check the process name below, then press Attach.";
    colour = "#ffcc66";
  }

  return (
    <>
      <PanelSectionRow>
        {attached ? (
          <div style={{ padding: "4px 0", fontSize: "0.8em" }}>
            Attached to <b>{process?.processName || "the game"}</b> ✓
          </div>
        ) : (
          <div style={{ padding: "4px 0", fontSize: "0.8em", color: colour }}>{status}</div>
        )}
      </PanelSectionRow>
      <PanelSectionRow>
        <TextField
          label="Process name"
          description={
            attachHint && attachHint === name
              ? "suggested by the trainer's sidecar — edit if it's wrong"
              : "e.g. game.exe"
          }
          value={name}
          onChange={(e) => {
            typed.current = true;
            setName(e.target.value);
          }}
        />
      </PanelSectionRow>
      <PanelSectionRow>
        <ButtonItem
          layout="below"
          bottomSeparator="none"
          disabled={!trimmed || redundant}
          onClick={() => onAttach(trimmed)}
        >
          {attached ? "Re-attach" : waiting ? "Attach again" : "Attach"}
        </ButtonItem>
      </PanelSectionRow>
    </>
  );
}
