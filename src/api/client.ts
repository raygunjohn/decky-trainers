// HATEOAS transport. Components never name a route: they hand an `action`
// straight out of the payload and we send it verbatim, through the backend
// proxy (the Steam webview is an https:// origin — plain fetch to
// http://127.0.0.1 risks mixed-content blocking).

import { apiRequest } from "./backend";
import type { Action, Feature, FeaturesPayload } from "./types";

export class ApiError extends Error {
  status: number;

  constructor(message: string, status = 0) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

async function request<T>(
  method: string,
  path: string,
  body?: Record<string, unknown> | null,
): Promise<T> {
  const result = await apiRequest(method, path, body ?? null);
  if (!result.ok) {
    throw new ApiError(result.error || `HTTP ${result.status}`, result.status);
  }
  return result.data as T;
}

export async function getFeatures(): Promise<FeaturesPayload> {
  return request<FeaturesPayload>("GET", "/api/features");
}

/**
 * Run one of a feature's advertised actions.
 *
 * The body is the action's own prefilled body with the caller's overrides on
 * top; the identifier is always replaced by the numeric `id`, because
 * descriptions are not unique (rdr2.ct ships two rows called "Pointer").
 */
export async function executeAction(
  action: Action,
  feature: Feature,
  overrides?: Record<string, unknown>,
): Promise<Feature> {
  const body: Record<string, unknown> = {
    ...(action.body ?? {}),
    ...(overrides ?? {}),
    id: feature.id,
  };
  delete body.description;
  return request<Feature>(action.method, action.path, body);
}

export async function attachProcess(
  processName: string,
): Promise<{ status: string; process?: unknown }> {
  return request("POST", "/api/process/attach", { process: processName });
}
