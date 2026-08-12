import { staticClasses } from "@decky/ui";
import { definePlugin } from "@decky/api";
import { FaBolt } from "react-icons/fa";

import { TrainerPanel } from "./components/TrainerPanel";
import { getTrainerStatus, stopTrainer } from "./api/backend";

export default definePlugin(() => {
  // Registered globally (not in the panel) so a trainer is torn down on game
  // exit even if the QAM is closed.
  const registration = SteamClient.GameSessions.RegisterForAppLifetimeNotifications(
    async (notification) => {
      if (notification.bRunning) return;
      try {
        const status = await getTrainerStatus();
        if (!status.running) return;
        console.log("[DeckyTrainers] game exited — stopping trainer");
        await stopTrainer();
      } catch (e) {
        console.error("[DeckyTrainers] failed to stop trainer on game exit", e);
      }
    },
  );

  return {
    name: "DeckyTrainers",
    titleView: <div className={staticClasses.Title}>DeckyTrainers</div>,
    content: <TrainerPanel />,
    icon: <FaBolt />,
    onDismount() {
      registration.unregister();
    },
  };
});
