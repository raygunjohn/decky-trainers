import { useCallback, useEffect, useState } from "react";

import { listInstalled } from "./backend";
import type { InstalledTrainer } from "./types";

export interface UseInstalled {
  trainers: InstalledTrainer[];
  trainerDir: string;
  loading: boolean;
  error: string | null;
  refresh: () => Promise<void>;
}

/**
 * The installed list, shared by both tabs: Generate needs it to warn before
 * overwriting a name, Installed renders it. One owner, so building or deleting
 * a trainer updates both views at once.
 */
export function useInstalled(): UseInstalled {
  const [trainers, setTrainers] = useState<InstalledTrainer[]>([]);
  const [trainerDir, setTrainerDir] = useState("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const result = await listInstalled();
      setTrainers(result.trainers ?? []);
      setTrainerDir(result.trainerDir ?? "");
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return { trainers, trainerDir, loading, error, refresh };
}
