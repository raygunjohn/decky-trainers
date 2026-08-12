// Shapes returned by the trainer's HATEOAS REST API (127.0.0.1:42069) and by
// the plugin's own Python backend (main.py).

export interface Action {
  method: string;
  path: string;
  body?: Record<string, unknown>;
}

export interface FeatureActions {
  toggle?: Action;
  setValue?: Action;
  [name: string]: Action | undefined;
}

export interface Feature {
  id: number;
  description: string;
  active: boolean;
  type: string;
  value?: string;
  isGroup?: boolean;
  /**
   * The table wants this record's children kept out of sight while it is off
   * (`moHideChildren`), or always (`moAlwaysHideChildren`).
   *
   * These are honoured through the treeview, which
   * is why its UI never shows a table's plumbing. rdr2.CT hides its whole
   * "BASE" layer this way — records the startup cascade legitimately activates.
   * They are active regardless; you just do not see them. See
   * `childrenHiddenByTable` in `tree.ts`.
   */
  hideChildren?: boolean;
  alwaysHideChildren?: boolean;
  actions?: FeatureActions;
  /**
   * Nested records. Absent (never `[]`) on leaves. Independent of `isGroup`:
   * a plain address row can carry children, and a group can have none.
   */
  children?: Feature[];
}

export interface ProcessBlock {
  attached: boolean;
  processId: number;
  processName: string;
  canSelfAttach: boolean;
}

export interface TrainerBlock {
  name?: string;
  exe?: string;
  table?: string;
}

export interface FeaturesPayload {
  process: ProcessBlock;
  endpoints?: Record<string, Action>;
  features: Feature[];
  trainer?: TrainerBlock;
  /**
   * The trainer is alive but its state was locked when we asked — it is
   * injecting a script (an AOB scan over the whole address space takes tens of
   * seconds). Everything else in the payload is absent, so this is the one
   * response the panel must *not* treat as data.
   */
  busy?: boolean;
}

// -- backend (main.py) ------------------------------------------------------

export interface BackendResult<T = unknown> {
  ok: boolean;
  status: number;
  data?: T;
  error?: string;
}

// -- generation -------------------------------------------------------------

/**
 * Whether this build can make a trainer at all, and whether the one it makes
 * will be able to attach. Everything the Generate tab needs to explain itself
 * before the user picks a file.
 */
export interface GeneratorStatus {
  ok: boolean;
  error?: string | null;
  /** Path to `bin/ct2trainer`, worth showing when it's the thing that's missing. */
  path: string;
  version: string;
  /** True when the plugin is running as root — which breaks launching, see `canAttach`. */
  isRoot: boolean;
  /**
   * False means a trainer would start and attach to nothing.
   *
   * A trainer runs under the game's Proton prefix, and `wine` started as root
   * joins root's wineserver instead of the game's — a session with no game in
   * it. So this is `!isRoot`: the plugin needs *less* privilege, not more.
   */
  canAttach: boolean;
}

export interface GenerateResult {
  ok: boolean;
  error?: string | null;
  path?: string;
  entry?: InstalledTrainer;
  /** The tail of ct2trainer's own output — where a table's parse error is named. */
  log?: string;
  seconds?: number;
}

/**
 * A trainer on disk. Generating one writes the binary plus a `<name>.json`
 * sidecar; these fields mirror that sidecar. Anything copied in by hand has no
 * sidecar and arrives as `local` with them empty.
 */
export interface InstalledTrainer {
  /** The file's path — nothing else about a local file is stable enough to key on. */
  id: string;
  gameName: string;
  processName: string;
  version: string;
  exe: string;
  path: string;
  sizeMB: number;
  installedAt: number;
  /** The table it was built from, when we know it. */
  ctPath: string;
  source: "generated" | "local";
  /**
   * Where the *table* came from, which is a different question from `source`
   * (where the trainer came from). `"github"` means it was downloaded rather
   * than picked; empty means picked, or unknown.
   *
   * Additive on purpose: a trainer built from a downloaded table is an
   * ordinary generated trainer, and widening `source` would make it sort and
   * render as a special case for no gain.
   */
  origin: "" | "github";
  /** `owner/repo`, when `origin` is `"github"`. */
  repo: string;
}

// -- remote tables ----------------------------------------------------------

/** One `.ct` blob in a GitHub repository. */
export interface RepoTable {
  /** Path within the repo — the id, and what `generateFromRepo` takes. */
  path: string;
  name: string;
  /** Containing folder, `""` at the repo root. */
  dir: string;
  size: number;
  /** Git blob sha, which is how the download is verified byte-for-byte. */
  sha: string;
  /** raw.githubusercontent URL, pinned to `commit` rather than to a branch. */
  url: string;
}

export interface RateLimit {
  limit: number;
  remaining: number;
  /** Unix seconds. Lets a rate-limit message say when it lifts. */
  reset: number;
}

export interface RepoListing {
  ok: boolean;
  error?: string | null;
  tables: RepoTable[];
  /** Echoed back so the field can be restored after the panel unmounts. */
  repoUrl: string;
  owner?: string;
  repo?: string;
  /** `owner/repo`. */
  label?: string;
  ref?: string;
  /** The commit the listing came from; every `url` above is pinned to it. */
  commit?: string;
  subdir?: string;
  /**
   * GitHub's tree endpoint gave up before the end of the repository, so this
   * list is genuinely partial. Never hide it: a silently short list of several
   * thousand tables cannot be falsified from a QAM panel.
   */
  truncated?: boolean;
  /** Served from disk rather than fetched — listing costs API budget. */
  cached?: boolean;
  /** The fetch failed and this is an older listing kept rather than nothing. */
  stale?: boolean;
  /** Why it is stale. Present with `stale`, and not an error — the list works. */
  warning?: string | null;
  fetchedAt?: number;
  rateLimit?: Partial<RateLimit>;
}

export interface InstalledListResult {
  trainers: InstalledTrainer[];
  /** Where both generated and hand-copied trainers live (`~/trainers`). */
  trainerDir: string;
}

export interface DeleteResult {
  ok: boolean;
  error?: string | null;
}

export interface GameContext {
  found: boolean;
  pid?: number;
  exeName?: string;
  prefix?: string;
  /**
   * The `wine` binary of the Proton build the game is running under — the one
   * a trainer must be launched through to land in the same wineserver session.
   * Empty when it could not be resolved, which is a launch blocker.
   */
  protonWine?: string;
}

/**
 * The process name to put in front of the user if attaching still needs a hand:
 * the name recorded when this trainer was generated, else the exe the backend
 * saw in `/proc`. Empty when neither is known.
 */
export interface AttachHint {
  attachHint?: string;
}

/** Launching never attaches — it only reports what to suggest attaching to. */
export interface LaunchResult extends AttachHint {
  ok: boolean;
  error?: string | null;
  gameExe?: string;
}

export interface DetectResult extends AttachHint {
  apiUp: boolean;
  trainerName?: string;
  attached: boolean;
  gameExe?: string;
}

export interface TrainerStatus extends AttachHint {
  running: boolean;
  apiUp: boolean;
  attached: boolean;
  gameExe: string;
  trainerName: string;
  adopted: boolean;
}

export interface StopResult {
  ok: boolean;
  /** Processes that had to be signalled. 0 means the trainer exited on request. */
  killed: number;
  apiUp: boolean;
  /**
   * Scripts the trainer restored before exiting, or `null` if it never
   * confirmed an un-patch (it was already gone, wedged, or had to be killed).
   *
   * `null` matters: it means the game may still carry this trainer's hooks, and
   * the failure shows up on the *next* injection — its AOB scans find their own
   * `jmp` instead of the pattern, and every script fails to enable.
   */
  unhooked?: number | null;
  error?: string | null;
}

export interface PluginSettings {
  port: number;
  trainerDir: string;
  /** Where the .CT picker opens — the folder the last table came from. */
  lastCtDir: string;
  /**
   * The last table picked. Held in the backend rather than component state
   * because opening the picker can unmount the QAM panel, which would drop it.
   */
  lastCtPath: string;
  /** The GitHub repository the Repo tab lists. One at a time, remembered. */
  repoUrl: string;
  /** Optional: private repos, and a shared IP that has spent the 60/hour. */
  githubToken: string;
  hideUnresolved: boolean;
}

/** The trainer prints this when it cannot read the value out of the game. */
export const UNRESOLVED = "??";

export function isUnresolved(value: string | undefined): boolean {
  return value === undefined || value.trim() === UNRESOLVED;
}
