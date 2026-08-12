import { useCallback, useEffect, useState } from "react";
import { ButtonItem, PanelSection, PanelSectionRow, Spinner } from "@decky/ui";

import { FeaturePanel } from "./features/FeaturePanel";
import { LauncherView } from "./LauncherView";
import { detectTrainer, getSettings, launchTrainer, setSetting, stopTrainer } from "../api/backend";
import { useRunningApp } from "../api/useRunningApp";
import { useTrainer } from "../api/useTrainer";
import type { InstalledTrainer } from "../api/types";

type Phase = "idle" | "launching" | "active" | "error";

export function TrainerPanel() {
  const game = useRunningApp();

  const [phase, setPhase] = useState<Phase>("idle");
  const [error, setError] = useState<string | null>(null);
  const [detecting, setDetecting] = useState(false);
  const [killing, setKilling] = useState(false);
  const [hideUnresolved, setHideUnresolved] = useState(true);
  const [gameExe, setGameExe] = useState("");
  // Survives launch and detect alike, so an adopted trainer gets it too.
  const [attachHint, setAttachHint] = useState("");

  const trainer = useTrainer(phase === "active");

  useEffect(() => {
    getSettings()
      .then((settings) => setHideUnresolved(settings.hideUnresolved !== false))
      .catch(() => undefined);
  }, []);

  const detect = useCallback(
    async (manual: boolean) => {
      setDetecting(true);
      if (manual) setError(null);
      try {
        const result = await detectTrainer();
        if (result.apiUp) {
          if (result.gameExe) setGameExe(result.gameExe);
          if (result.attachHint) setAttachHint(result.attachHint);
          setPhase("active");
        } else if (manual) {
          setError("No trainer answering on port 42069.");
        }
      } catch (e) {
        if (manual) setError(e instanceof Error ? e.message : String(e));
      } finally {
        setDetecting(false);
      }
    },
    [],
  );

  // Covers trainers we didn't launch: started over SSH, or still alive after a
  // plugin reload wiped the backend's state.
  useEffect(() => {
    detect(false);
  }, [detect]);

  const launch = useCallback(
    async (file: InstalledTrainer) => {
      if (!game) return;
      setPhase("launching");
      setError(null);
      try {
        const result = await launchTrainer(game.appid, file.path);
        if (result.ok) {
          setGameExe(result.gameExe ?? "");
          // The record's own process name beats whatever the backend echoes.
          setAttachHint(result.attachHint || file.processName || "");
          setPhase("active");
        } else {
          setError(result.error ?? "Launch failed.");
          setPhase("error");
        }
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
        setPhase("error");
      }
    },
    [game],
  );

  const kill = useCallback(async () => {
    setKilling(true);
    try {
      const result = await stopTrainer();
      if (result.ok) {
        setError(null);
        setPhase("idle");
      } else {
        setError(result.error ?? "The trainer is still running.");
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setKilling(false);
    }
  }, []);

  const onHideUnresolvedChange = useCallback((hide: boolean) => {
    setHideUnresolved(hide);
    setSetting("hideUnresolved", hide).catch(() => undefined);
  }, []);

  if (phase === "launching") {
    return (
      <PanelSection title="Starting trainer">
        <PanelSectionRow>
          <div style={{ display: "flex", alignItems: "center", gap: "8px", padding: "8px 0" }}>
            <Spinner style={{ width: "20px", height: "20px" }} />
            <span style={{ fontSize: "0.8em" }}>Starting trainer…</span>
          </div>
        </PanelSectionRow>
      </PanelSection>
    );
  }

  if (phase === "error") {
    return (
      <PanelSection title="Launch failed">
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em", color: "#ff6b6b" }}>{error}</div>
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem layout="below" onClick={() => setPhase("idle")}>
            Back
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>
    );
  }

  if (phase === "active") {
    return (
      <>
        <PanelSection>
          <PanelSectionRow>
            <ButtonItem layout="below" disabled={killing} onClick={kill}>
              <span style={{ color: "#ff6b6b", fontWeight: "bold" }}>
                {killing ? "Killing…" : "KILL TRAINER"}
              </span>
            </ButtonItem>
          </PanelSectionRow>
          {trainer.busy && (
            <PanelSectionRow>
              <div style={{ padding: "4px 0", fontSize: "0.8em", color: "#66ccff" }}>
                Injecting — scanning the game's memory. This can take a minute;
                don't kill the trainer.
              </div>
            </PanelSectionRow>
          )}
          {!trainer.online && !trainer.loading && (
            <>
              <PanelSectionRow>
                <div style={{ padding: "4px 0", fontSize: "0.8em", color: "#ffcc66" }}>
                  Trainer not answering — retrying…
                </div>
              </PanelSectionRow>
              <PanelSectionRow>
                <ButtonItem layout="below" onClick={() => setPhase("idle")}>
                  Back to launcher
                </ButtonItem>
              </PanelSectionRow>
            </>
          )}
        </PanelSection>
        <FeaturePanel
          features={trainer.features}
          process={trainer.process}
          gameExe={gameExe || trainer.process?.processName}
          attachHint={attachHint}
          attachStatus={trainer.attachStatus}
          loading={trainer.loading}
          error={error ?? trainer.error}
          hideUnresolved={hideUnresolved}
          onHideUnresolvedChange={onHideUnresolvedChange}
          onToggle={trainer.toggle}
          onSetValue={trainer.setValue}
          onAttach={trainer.attach}
        />
      </>
    );
  }

  return (
    <LauncherView
      game={game}
      detecting={detecting}
      error={error}
      onLaunch={launch}
      onDetect={() => detect(true)}
    />
  );
}
