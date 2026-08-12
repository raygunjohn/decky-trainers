import { useEffect, useRef, useState } from "react";
import { DialogButton, Focusable, PanelSection, PanelSectionRow } from "@decky/ui";

import { GenerateTab } from "./GenerateTab";
import { InstalledList } from "./InstalledList";
import { RepoTab } from "./RepoTab";
import { useInstalled } from "../../api/useInstalled";
import type { InstalledTrainer } from "../../api/types";

/**
 * `local` and `repo` are two *sources* for a table, not two ways to build one:
 * both end at a path on disk and go through the same generator. `local` is the
 * tab that used to be called Generate.
 */
type Tab = "local" | "repo" | "installed";

export interface TrainerBrowserProps {
  /** Seeds the new trainer's name — the running game is almost always what's wanted. */
  gameName?: string;
  /** Lets the Generate tab pre-fill the process name from `/proc`. */
  appid?: string;
  canLaunch: boolean;
  onLaunch: (trainer: InstalledTrainer) => void;
}

/**
 * Local (a .CT on the Deck), Repo (a .CT in a GitHub repository) and Installed
 * (what's already built), sharing one installed list so a build from either
 * source lands in the third tab immediately.
 *
 * `Tabs` from @decky/ui is built for full-width library pages; at ~300px its
 * header truncates, so the switcher is buttons instead — and with three of
 * them the labels have to stay one short word each.
 */
export function TrainerBrowser({ gameName, appid, canLaunch, onLaunch }: TrainerBrowserProps) {
  const installed = useInstalled();
  const [tab, setTab] = useState<Tab>("local");
  const chosen = useRef(false);

  // Someone with trainers already on disk wants to play, not to build another —
  // but the list only arrives after the first render, and a manual pick wins.
  useEffect(() => {
    if (chosen.current || installed.loading) return;
    chosen.current = true;
    if (installed.trainers.length > 0) setTab("installed");
  }, [installed.loading, installed.trainers.length]);

  const pick = (id: Tab) => {
    chosen.current = true;
    setTab(id);
  };

  const built = () => {
    installed.refresh();
    pick("installed");
  };

  const tabButton = (id: Tab, label: string) => (
    <DialogButton
      style={{
        flex: 1,
        minWidth: 0,
        padding: "8px 4px",
        fontSize: "0.85em",
        opacity: tab === id ? 1 : 0.55,
      }}
      onClick={() => pick(id)}
    >
      {label}
    </DialogButton>
  );

  const count = installed.trainers.length;

  return (
    <PanelSection title="Trainers">
      <PanelSectionRow>
        <Focusable style={{ display: "flex", gap: "6px", padding: "4px 0" }}>
          {tabButton("local", "Local")}
          {tabButton("repo", "Repo")}
          {tabButton("installed", count ? `Installed (${count})` : "Installed")}
        </Focusable>
      </PanelSectionRow>

      {installed.error && (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em", color: "#ff6b6b" }}>
            {installed.error}
          </div>
        </PanelSectionRow>
      )}

      {/* A fresh trainer is there to be launched, and the Installed tab is the
          only place that can do it — from either source. */}
      {tab === "local" ? (
        <GenerateTab
          gameName={gameName}
          appid={appid}
          existingNames={installed.trainers.map((t) => t.exe)}
          onBuilt={built}
        />
      ) : tab === "repo" ? (
        <RepoTab
          gameName={gameName}
          appid={appid}
          existingNames={installed.trainers.map((t) => t.exe)}
          onBuilt={built}
        />
      ) : (
        <InstalledList
          trainers={installed.trainers}
          loading={installed.loading}
          canLaunch={canLaunch}
          trainerDir={installed.trainerDir}
          onLaunch={onLaunch}
          onDeleted={installed.refresh}
        />
      )}
    </PanelSection>
  );
}
