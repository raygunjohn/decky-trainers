import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ButtonItem,
  DialogButton,
  Focusable,
  PanelSectionRow,
  Spinner,
  TextField,
} from "@decky/ui";
import { FaSyncAlt } from "react-icons/fa";

import { generateFromRepo, getSettings, setSetting } from "../../api/backend";
import { searchTables } from "../../api/repoSearch";
import { useGenerator } from "../../api/useGenerator";
import { useRepoTables } from "../../api/useRepoTables";
import type { RepoTable } from "../../api/types";
import { BuildForm } from "./BuildForm";

export interface RepoTabProps {
  /** Seeds the new trainer's name *and* the search — see `seeded` below. */
  gameName?: string;
  /** Seeds the process name from what the backend sees in `/proc`. */
  appid?: string;
  existingNames: string[];
  onBuilt: () => void;
}

const WARN = "#ffcc66";
const BAD = "#ff6b6b";
const DIM = 0.6;

/**
 * A ~300px panel cannot show 5606 rows, and scrolling to one on a thumbstick
 * is not a plan. This is a search UI with a browsable head, not a list.
 */
const MAX_RESULTS = 40;

function sizeLabel(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  if (bytes >= 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${bytes} B`;
}

/** `tables/Dark Souls/ds1.ct` -> `Dark Souls`; root tables get no subtitle. */
function folderLabel(table: RepoTable): string {
  if (!table.dir) return "";
  return table.dir.split("/").pop() ?? table.dir;
}

/**
 * Build a trainer from a `.CT` in a **GitHub repository**.
 *
 * The sibling of `GenerateTab`, answering the same question with a table that
 * is not on the Deck yet. Everything past choosing one is shared: the backend
 * downloads into a cache and hands the path to the same generator, and this
 * tab renders the same `BuildForm`.
 */
export function RepoTab({ gameName, appid, existingNames, onBuilt }: RepoTabProps) {
  const status = useGenerator();
  const repo = useRepoTables();

  const [url, setUrl] = useState("");
  const [query, setQuery] = useState("");
  const [chosen, setChosen] = useState<RepoTable | null>(null);
  const [token, setToken] = useState("");
  const [showToken, setShowToken] = useState(false);

  // Both of these arrive after mount and both are editable, so neither can be
  // a `useState(prop)` — that keeps the first render's value forever. This bug
  // class has bitten this repo twice; the effect+ref pattern is the fix.
  const urlTyped = useRef(false);
  const queryTyped = useRef(false);

  useEffect(() => {
    if (!urlTyped.current && repo.repoUrl) setUrl(repo.repoUrl);
  }, [repo.repoUrl]);

  useEffect(() => {
    getSettings()
      .then((settings) => {
        // Only reveal the field when there is already a token to show; for
        // everyone else it stays behind the tap, since the 60-an-hour budget
        // covers 30 listings and almost nobody needs this.
        if (settings.githubToken) {
          setToken(settings.githubToken);
          setShowToken(true);
        }
      })
      .catch(() => undefined);
  }, []);

  // The ergonomic point of this tab. `Router.MainRunningApp` settles late, so
  // by the time the game's name is known the panel is already up — and landing
  // pre-filtered to the game someone is playing is the difference between one
  // tap and typing a name on an on-screen keyboard.
  useEffect(() => {
    if (!queryTyped.current && gameName) setQuery(gameName);
  }, [gameName]);

  const result = useMemo(
    () => searchTables(repo.tables, query, MAX_RESULTS),
    [repo.tables, query],
  );

  const build = useCallback(
    (name: string, processName: string) =>
      generateFromRepo(chosen?.path ?? "", name, processName),
    [chosen],
  );

  const listed = repo.tables.length;

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
        <TextField
          label="GitHub repository"
          description={
            listed
              ? `${repo.label}@${repo.ref} · ${listed} tables`
              : "e.g. github.com/owner/repo"
          }
          value={url}
          bShowClearAction
          disabled={repo.loading}
          onChange={(e) => {
            urlTyped.current = true;
            setUrl(e.target.value);
          }}
        />
      </PanelSectionRow>

      <PanelSectionRow>
        <Focusable style={{ display: "flex", gap: "6px", padding: "4px 0" }}>
          <DialogButton
            style={{ flex: 1, minWidth: 0, padding: "8px 4px", fontSize: "0.85em" }}
            disabled={repo.loading || !url.trim()}
            onClick={() => {
              setChosen(null);
              repo.load(url.trim(), false);
            }}
          >
            {listed ? "List tables" : "Load"}
          </DialogButton>
          {/* Refetching spends 2 of the 60 API calls an unauthenticated IP gets
              per hour, so it is a deliberate tap rather than something that
              happens whenever the panel opens. */}
          <DialogButton
            style={{ minWidth: 0, width: "40px", padding: "8px 0" }}
            disabled={repo.loading || !listed}
            onClick={() => {
              setChosen(null);
              repo.refresh();
            }}
          >
            <FaSyncAlt />
          </DialogButton>
        </Focusable>
      </PanelSectionRow>

      {repo.loading && (
        <PanelSectionRow>
          <div style={{ display: "flex", alignItems: "center", gap: "8px", padding: "8px 0" }}>
            <Spinner style={{ width: "16px", height: "16px" }} />
            <span style={{ fontSize: "0.8em" }}>Listing the repository…</span>
          </div>
        </PanelSectionRow>
      )}

      {/* The two failures a token fixes are exactly the two this offers it for,
          so it appears next to the message rather than in a settings page
          nobody would think to open. */}
      {!showToken && (
        <PanelSectionRow>
          <ButtonItem layout="below" onClick={() => setShowToken(true)}>
            <span style={{ fontSize: "0.75em", opacity: DIM }}>
              Private repo, or rate limited? Add a GitHub token
            </span>
          </ButtonItem>
        </PanelSectionRow>
      )}

      {showToken && (
        <PanelSectionRow>
          <TextField
            label="GitHub token"
            description="Optional — for private repositories and a bigger API budget"
            value={token}
            bIsPassword
            bShowClearAction
            onChange={(e) => {
              const next = e.target.value;
              setToken(next);
              // Written straight through: opening the on-screen keyboard can
              // unmount this panel, and a token that only lived in React would
              // be gone when it came back.
              setSetting("githubToken", next.trim()).catch(() => undefined);
            }}
          />
        </PanelSectionRow>
      )}

      {repo.error && (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em", color: BAD }}>{repo.error}</div>
        </PanelSectionRow>
      )}

      {/* Stale is not an error: these tables are still downloadable, and the
          panel would be worse for being empty. */}
      {repo.stale && repo.warning && (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em", color: WARN }}>
            Showing an older list — {repo.warning}
          </div>
        </PanelSectionRow>
      )}

      {/* Above GitHub's tree ceiling the listing is genuinely partial. Saying
          so is the only way anyone could tell. */}
      {repo.truncated && (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em", color: WARN }}>
            This repository is too large to list completely — some tables are missing.
          </div>
        </PanelSectionRow>
      )}

      {!repo.loading && !repo.error && repo.repoUrl && listed === 0 && (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em", opacity: DIM }}>
            No .CT files in this repository. Some projects keep their table under
            Releases instead of in the repo itself.
          </div>
        </PanelSectionRow>
      )}

      {listed > 0 && (
        <PanelSectionRow>
          <TextField
            label="Search tables"
            value={query}
            bShowClearAction
            onChange={(e) => {
              queryTyped.current = true;
              setQuery(e.target.value);
            }}
          />
        </PanelSectionRow>
      )}

      {listed > 0 && (
        <PanelSectionRow>
          <div style={{ padding: "2px 0", fontSize: "0.75em", opacity: DIM }}>
            {result.total === 0
              ? `Nothing matches “${query}”`
              : result.total > result.tables.length
                ? `${result.total} matches — showing the top ${result.tables.length}`
                : `${result.total} ${result.total === 1 ? "table" : "tables"}`}
          </div>
        </PanelSectionRow>
      )}

      {/* `ButtonItem` rather than a focusable `Field`: it is what every other
          tappable row in this panel is (see `ExpandRow`, `FeatureRow`), and a
          row that cannot be reached with a thumbstick would make this whole
          tab unusable in the one place it has to work. */}
      {result.tables.map((table) => {
        const isChosen = chosen?.path === table.path;
        const folder = folderLabel(table);
        return (
          <Fragment key={table.path}>
            <PanelSectionRow>
              <ButtonItem
                layout="below"
                bottomSeparator="none"
                description={[folder, sizeLabel(table.size)].filter(Boolean).join(" · ")}
                onClick={() => setChosen(isChosen ? null : table)}
              >
                <span style={{ wordBreak: "break-word" }}>
                  {isChosen ? "▾ " : "▸ "}
                  {table.name}
                </span>
              </ButtonItem>
            </PanelSectionRow>

            {/* The form opens under the row it belongs to, so what is being
                built stays on screen next to the name being typed for it. */}
            {isChosen && (
              <BuildForm
                gameName={gameName}
                appid={appid}
                suggestedName={table.name.replace(/\.ct$/i, "")}
                existingNames={existingNames}
                build={build}
                onBuilt={onBuilt}
              />
            )}
          </Fragment>
        );
      })}
    </>
  );
}
