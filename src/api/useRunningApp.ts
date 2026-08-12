// Which game is running, straight from the Steam client — no polling of our own
// beyond a cheap re-read of Router.MainRunningApp on lifetime events.

import { useEffect, useState } from "react";
import { Router } from "@decky/ui";
import type { AppOverview } from "@decky/ui";

export function useRunningApp(): AppOverview | undefined {
  const [app, setApp] = useState<AppOverview | undefined>(() => Router.MainRunningApp);

  useEffect(() => {
    const read = () => setApp(Router.MainRunningApp);
    read();

    const registration = SteamClient.GameSessions.RegisterForAppLifetimeNotifications(() => {
      // MainRunningApp settles a moment after the notification fires.
      setTimeout(read, 500);
    });
    // The QAM can open long after the game started; a slow tick keeps the row
    // honest if a lifetime event was missed.
    const interval = setInterval(read, 5000);

    return () => {
      registration.unregister();
      clearInterval(interval);
    };
  }, []);

  return app;
}
