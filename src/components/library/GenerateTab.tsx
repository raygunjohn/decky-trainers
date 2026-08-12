import { useCallback, useEffect, useState } from "react";
import { DialogButton, Field, PanelSectionRow, Spinner } from "@decky/ui";
import { FaFolderOpen } from "react-icons/fa";
import { FileSelectionType, openFilePicker } from "@decky/api";

import { generateTrainer, getSettings, setSetting } from "../../api/backend";
import { useGenerator } from "../../api/useGenerator";
import { BuildForm } from "./BuildForm";

export interface GenerateTabProps {
  /** Seeds the new trainer's name — the running game is almost always what's wanted. */
  gameName?: string;
  /** Seeds the process name from what the backend sees in `/proc`. */
  appid?: string;
  /** Names already in `~/trainers`, so building over one can be called out first. */
  existingNames: string[];
  onBuilt: () => void;
}

const WARN = "#ffcc66";
const BAD = "#ff6b6b";

/** Where tables land on a Deck. Matches `DEFAULT_CT_DIR` in main.py. */
const DEFAULT_CT_DIR = "/home/deck/Downloads";

/** `~/Downloads/Kenshi_x64.ct` -> `Kenshi_x64`. */
function tableName(path: string): string {
  const base = path.split("/").pop() ?? path;
  return base.replace(/\.ct$/i, "");
}

/**
 * Turn any `.CT` **on the Deck** into a trainer.
 *
 * Tables come from wherever the player got them — browser, scp, USB stick — and
 * everything downstream is unchanged from when a trainer arrived prebuilt.
 * `RepoTab` is this tab's sibling, not its replacement: it answers the same
 * question with a table from a GitHub repository, and both hand a path on disk
 * to the same `BuildForm`.
 */
export function GenerateTab({ gameName, appid, existingNames, onBuilt }: GenerateTabProps) {
  const status = useGenerator();
  const [ctPath, setCtPath] = useState("");
  // Seeded, not empty: `getSettings` resolves after mount, and tapping the
  // folder button before it lands used to open the picker at `/home/deck` —
  // which has no tables in it, so the picker came up empty.
  const [startPath, setStartPath] = useState(DEFAULT_CT_DIR);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getSettings()
      .then((settings) => {
        if (settings.lastCtDir) setStartPath(settings.lastCtDir);
        // Restores the pick across the unmount described below.
        if (settings.lastCtPath) setCtPath(settings.lastCtPath);
      })
      .catch(() => undefined);
  }, []);

  const pick = useCallback(async () => {
    setError(null);
    try {
      const picked = await openFilePicker(
        FileSelectionType.FILE,
        startPath || DEFAULT_CT_DIR,
        true, // files
        true, // folders — without this there is no way to navigate anywhere,
        //       so a start path with no tables in it is a dead end
        undefined, // extensions alone; a second filter only adds ways to hide
        //            a file the user can see is there
        ["ct", "CT"], // the list is case-sensitive and tables ship as both
        false, // hidden files
        true, // ...but always leave "All files" reachable: a table named
        //       `.CT3` or `.ct.txt` is still a table, and a picker that
        //       silently shows nothing is unfixable from the QAM
      );
      const chosen = picked?.path || picked?.realpath;
      if (!chosen) {
        // Never fail silently here: a picker that closes and leaves the panel
        // exactly as it was is indistinguishable from a broken button, and
        // there is no console to check from Gaming Mode.
        setError(`The picker returned no file (${JSON.stringify(picked ?? null)}).`);
        return;
      }

      const directory = chosen.slice(0, chosen.lastIndexOf("/"));

      // Persisted *before* the state updates, and deliberately not awaited into
      // the render path: opening a modal from the QAM can unmount this panel,
      // and a `setCtPath` on an unmounted component is a silent no-op — the
      // pick is simply lost and the panel comes back blank. The backend is the
      // only place a choice survives that, so it is written first and read back
      // on mount. (It is also how this gets diagnosed: the setting lands even
      // when nothing renders.)
      setSetting("lastCtPath", chosen).catch(() => undefined);
      if (directory) setSetting("lastCtDir", directory).catch(() => undefined);

      setCtPath(chosen);
      setStartPath(directory || startPath);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [startPath]);

  const build = useCallback(
    (name: string, processName: string) => generateTrainer(ctPath, name, processName),
    [ctPath],
  );

  if (status && !status.ok) {
    return (
      <PanelSectionRow>
        <div style={{ padding: "8px 0", fontSize: "0.8em", color: BAD }}>
          {status.error ?? "The trainer generator is unavailable."}
        </div>
      </PanelSectionRow>
    );
  }

  return (
    <>
      {status && !status.canAttach && (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em", color: WARN }}>
            The plugin is running as root, so a trainer launched from here would get
            its own Wine session and never see the game. Remove the “root” flag from
            plugin.json.
          </div>
        </PanelSectionRow>
      )}

      <PanelSectionRow>
        <Field
          label={ctPath ? tableName(ctPath) : "Cheat table"}
          description={ctPath ? "Choose a different table" : "Pick a .CT file to build from"}
          padding="compact"
          bottomSeparator="none"
          childrenLayout="inline"
          childrenContainerWidth="min"
        >
          <DialogButton
            style={{ minWidth: 0, width: "40px", padding: "8px 0" }}
            onClick={pick}
          >
            <FaFolderOpen />
          </DialogButton>
        </Field>
      </PanelSectionRow>

      {ctPath && (
        <BuildForm
          gameName={gameName}
          appid={appid}
          suggestedName={tableName(ctPath)}
          existingNames={existingNames}
          build={build}
          onBuilt={onBuilt}
        />
      )}

      {error && (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em", color: BAD }}>{error}</div>
        </PanelSectionRow>
      )}

      {!status && (
        <PanelSectionRow>
          <div style={{ display: "flex", alignItems: "center", gap: "8px", padding: "8px 0" }}>
            <Spinner style={{ width: "16px", height: "16px" }} />
            <span style={{ fontSize: "0.8em" }}>Checking the generator…</span>
          </div>
        </PanelSectionRow>
      )}
    </>
  );
}
