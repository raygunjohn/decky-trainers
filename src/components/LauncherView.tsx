import { ButtonItem, PanelSection, PanelSectionRow } from "@decky/ui";
import type { AppOverview } from "@decky/ui";

import { TrainerBrowser } from "./library/TrainerBrowser";
import type { InstalledTrainer } from "../api/types";

export interface LauncherViewProps {
  game?: AppOverview;
  detecting: boolean;
  error: string | null;
  onLaunch: (trainer: InstalledTrainer) => void;
  onDetect: () => void;
}

export function LauncherView({
  game,
  detecting,
  error,
  onLaunch,
  onDetect,
}: LauncherViewProps) {
  return (
    <>
      <PanelSection title={game ? "Playing" : "No game running"}>
        <PanelSectionRow>
          <div style={{ padding: "4px 0" }}>
            {game ? (
              <b>{game.display_name}</b>
            ) : (
              <span style={{ fontSize: "0.8em" }}>
                Start a game first — a trainer has to have something to attach to.
              </span>
            )}
          </div>
        </PanelSectionRow>
        {error && (
          <PanelSectionRow>
            <div style={{ padding: "4px 0", fontSize: "0.8em", color: "#ff6b6b" }}>{error}</div>
          </PanelSectionRow>
        )}
        <PanelSectionRow>
          <ButtonItem layout="below" disabled={detecting} onClick={onDetect}>
            {detecting ? "Probing…" : "Detect running trainer"}
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>

      {/* Building a trainer works with no game running; only launching needs
          one, so the library stays reachable either way. */}
      <TrainerBrowser
        gameName={game?.display_name}
        appid={game?.appid !== undefined ? String(game.appid) : undefined}
        canLaunch={Boolean(game)}
        onLaunch={onLaunch}
      />
    </>
  );
}
