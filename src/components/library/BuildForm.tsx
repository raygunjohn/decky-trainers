import { useCallback, useEffect, useRef, useState } from "react";
import { ButtonItem, PanelSectionRow, Spinner, TextField } from "@decky/ui";
import { FaHammer } from "react-icons/fa";

import { getGameContext } from "../../api/backend";
import type { GenerateResult } from "../../api/types";

export interface BuildFormProps {
  /** Seeds the trainer's name — the running game is almost always what's wanted. */
  gameName?: string;
  /** Seeds the process name from what the backend sees in `/proc`. */
  appid?: string;
  /** The chosen table's name, used when there is no running game to name it after. */
  suggestedName?: string;
  /** Names already in `~/trainers`, so building over one can be called out first. */
  existingNames: string[];
  /** Where the table came from. The two callers differ in exactly this. */
  build: (name: string, processName: string) => Promise<GenerateResult>;
  onBuilt: () => void;
}

const BAD = "#ff6b6b";

/**
 * What `library.safe_filename` will make of a typed name — mirrored here so the
 * "replaces an existing trainer" warning compares the filename that will
 * actually be written, not the text in the box (`Red Dead 2` lands as
 * `Red_Dead_2`). The backend still sanitises; this is only ever advisory.
 *
 * The trailing `.exe` is stripped for the same reason it is stripped there: a
 * name the user typed must never supply its own extension and land as
 * `Foo.exe.exe`. Callers add the suffix back, so what this returns is compared
 * against `${filename}.exe` — the installed list is keyed by full filename, and
 * comparing the bare name against it silently matched nothing.
 */
export function safeName(raw: string): string {
  const base = (raw ?? "").replace(/\\/g, "/").split("/").pop() ?? "";
  const cleaned = base.trim().replace(/[^A-Za-z0-9._-]+/g, "_").replace(/^\.+/, "");
  return cleaned.replace(/\.exe$/i, "");
}

/**
 * Everything between choosing a table and having a trainer: the name, the
 * process to attach to, the button, and the build output.
 *
 * There is one of these because there is one build path. A table picked off
 * the Deck and a table downloaded from a repo differ only in how they became a
 * path on disk — `build` is that difference and nothing else is. Two copies of
 * this markup would drift, and the first thing to drift would be the "Show
 * build output" toggle, which is the only place a table that will not parse
 * ever names the record that broke it.
 */
export function BuildForm({
  gameName,
  appid,
  suggestedName,
  existingNames,
  build,
  onBuilt,
}: BuildFormProps) {
  const [name, setName] = useState("");
  const [processName, setProcessName] = useState("");
  const [building, setBuilding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [log, setLog] = useState("");
  const [showLog, setShowLog] = useState(false);

  // Every seed here arrives asynchronously and every field is editable, so
  // none of them can be a `useState(prop)` — that keeps the first render's
  // value forever. The refs mean anything typed, including clearing a field,
  // outranks a later seed.
  const namedByUser = useRef(false);
  const processTypedByUser = useRef(false);

  useEffect(() => {
    if (!namedByUser.current && gameName) setName(gameName);
  }, [gameName]);

  // Only when there is no running game to name the trainer after: the game
  // beats the filename, which is often a version-stamped upload name.
  useEffect(() => {
    if (!namedByUser.current && !gameName && suggestedName) setName(suggestedName);
  }, [gameName, suggestedName]);

  // The exe the backend found in /proc — what the trainer will have to attach
  // to, and the exact string this plugin exists to stop people typing.
  useEffect(() => {
    if (!appid) return;
    let cancelled = false;
    getGameContext(appid)
      .then((context) => {
        if (cancelled || processTypedByUser.current) return;
        if (context.found && context.exeName) setProcessName(context.exeName);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [appid]);

  const run = useCallback(async () => {
    setBuilding(true);
    setError(null);
    setLog("");
    try {
      const result = await build(name.trim(), processName.trim());
      setLog(result.log ?? "");
      if (result.ok) onBuilt();
      else setError(result.error ?? "The trainer could not be built.");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBuilding(false);
    }
  }, [build, name, onBuilt, processName]);

  const filename = safeName(name);
  const collides = Boolean(filename) && existingNames.includes(`${filename}.exe`);
  const renamed = Boolean(filename) && filename !== name.trim();

  return (
    <>
      <PanelSectionRow>
        <TextField
          label="Trainer name"
          description={
            collides
              ? `Replaces the trainer already saved as ${filename}.exe`
              : renamed
                ? `Saved as ${filename}.exe`
                : undefined
          }
          value={name}
          bShowClearAction
          disabled={building}
          onChange={(e) => {
            namedByUser.current = true;
            setName(e.target.value);
          }}
        />
      </PanelSectionRow>

      <PanelSectionRow>
        <TextField
          label="Game process"
          description="What the trainer attaches to — pre-filled from the running game"
          value={processName}
          bShowClearAction
          disabled={building}
          onChange={(e) => {
            processTypedByUser.current = true;
            setProcessName(e.target.value);
          }}
        />
      </PanelSectionRow>

      <PanelSectionRow>
        <ButtonItem layout="below" disabled={building || !name.trim()} onClick={run}>
          {building ? (
            <span style={{ display: "flex", alignItems: "center", gap: "8px" }}>
              <Spinner style={{ width: "16px", height: "16px" }} />
              Building…
            </span>
          ) : (
            <span style={{ display: "flex", alignItems: "center", gap: "8px" }}>
              <FaHammer />
              Build trainer
            </span>
          )}
        </ButtonItem>
      </PanelSectionRow>

      {/* Measured on a Deck: 11 ms for a 246 KB table, 97 ms for the 1.9 MB RDR2
          one. So this is a flicker, not a progress report — if it ever stays up,
          something is wrong, which is worth saying rather than reassuring.
          A repo build adds a download to that, which is why it says so. */}
      {building && (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em" }}>Building from the table…</div>
        </PanelSectionRow>
      )}

      {error && (
        <PanelSectionRow>
          <div style={{ padding: "4px 0", fontSize: "0.8em", color: BAD }}>{error}</div>
        </PanelSectionRow>
      )}

      {/* A table that won't parse is the common failure, and the generator's own
          output is the only thing that names the record that broke it — but it's
          noise until something goes wrong, so it stays behind a tap. */}
      {log && (
        <PanelSectionRow>
          <ButtonItem layout="below" onClick={() => setShowLog((shown) => !shown)}>
            <span style={{ fontSize: "0.8em" }}>
              {showLog ? "Hide build output" : "Show build output"}
            </span>
          </ButtonItem>
        </PanelSectionRow>
      )}

      {log && showLog && (
        <PanelSectionRow>
          <div
            style={{
              padding: "4px 0",
              fontSize: "0.7em",
              fontFamily: "monospace",
              whiteSpace: "pre-wrap",
              wordBreak: "break-word",
              opacity: 0.8,
            }}
          >
            {log}
          </div>
        </PanelSectionRow>
      )}
    </>
  );
}
