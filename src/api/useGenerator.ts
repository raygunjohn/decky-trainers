import { useEffect, useState } from "react";

import { checkGenerator } from "./backend";
import type { GeneratorStatus } from "./types";

/**
 * Whether this build can make a trainer at all, and whether the one it makes
 * could attach.
 *
 * Both build tabs need the same answer and both must be able to say so
 * *before* anyone picks a table — "nothing happened when I chose a file" is
 * the one failure that cannot be diagnosed from the QAM. Kept as a hook rather
 * than lifted into `TrainerBrowser` so each tab's unavailable state stays its
 * own early return: without a generator, a file picker and a repo listing are
 * equally pointless, but they are pointless in different words.
 */
export function useGenerator(): GeneratorStatus | null {
  const [status, setStatus] = useState<GeneratorStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    checkGenerator()
      .then((result) => {
        if (!cancelled) setStatus(result);
      })
      .catch((e) => {
        if (cancelled) return;
        setStatus({
          ok: false,
          error: e instanceof Error ? e.message : String(e),
          path: "",
          version: "",
          isRoot: false,
          canAttach: false,
        });
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return status;
}
