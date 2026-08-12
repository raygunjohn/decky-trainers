// Typed wrappers over the callables exposed by main.py.

import { callable } from "@decky/api";
import type {
  BackendResult,
  DeleteResult,
  DetectResult,
  GameContext,
  GenerateResult,
  GeneratorStatus,
  InstalledListResult,
  LaunchResult,
  PluginSettings,
  RepoListing,
  StopResult,
  TrainerStatus,
} from "./types";

/** Whether `bin/ct2trainer` is there, runnable, and able to produce a trainer that attaches. */
export const checkGenerator = callable<[], GeneratorStatus>("check_generator");

/**
 * Build a trainer from a cheat table into `~/trainers`. Slow — seconds
 * for a large table — so callers need a busy state, not just a disabled button.
 */
export const generateTrainer = callable<
  [ctPath: string, name: string, processName: string],
  GenerateResult
>("generate_trainer");

/**
 * Every `.ct` in a GitHub repository, recursively. Served from an on-disk
 * cache unless `refresh` is set — listing spends 2 of the 60 API calls an
 * unauthenticated IP gets per hour.
 */
export const listRepoTables = callable<[url: string, refresh: boolean], RepoListing>(
  "list_repo_tables",
);

/**
 * Download one listed table and build a trainer from it. Takes the table's
 * path within the repo; the backend resolves it against the cached listing, so
 * the file built is the one that was on screen.
 */
export const generateFromRepo = callable<
  [path: string, name: string, processName: string],
  GenerateResult
>("generate_from_repo");

export const clearTableCache = callable<[], { ok: boolean; freedMB: number }>(
  "clear_table_cache",
);

export const listInstalled = callable<[], InstalledListResult>("list_installed");

export const deleteTrainer = callable<[trainerId: string, path: string], DeleteResult>(
  "delete_trainer",
);

export const getGameContext = callable<[appid: string], GameContext>("get_game_context");

export const launchTrainer = callable<[appid: string, trainerPath: string], LaunchResult>(
  "launch_trainer",
);

export const detectTrainer = callable<[], DetectResult>("detect_trainer");

export const getTrainerStatus = callable<[], TrainerStatus>("get_trainer_status");

export const stopTrainer = callable<[], StopResult>("stop_trainer");

export const apiRequest = callable<
  [method: string, path: string, body?: Record<string, unknown> | null],
  BackendResult
>("api_request");

export const getSettings = callable<[], PluginSettings>("get_settings");

export const setSetting = callable<[key: string, value: unknown], PluginSettings>("set_setting");
