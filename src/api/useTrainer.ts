// Polling + mutation hook for the trainer API. Bound to the panel lifecycle:
// polling starts on mount and stops on unmount, so a closed QAM costs nothing.

import { useCallback, useEffect, useRef, useState } from "react";

import { attachProcess, executeAction, getFeatures } from "./client";
import type { Feature, FeaturesPayload, ProcessBlock, TrainerBlock } from "./types";

const POLL_ONLINE_MS = 2000;
const POLL_OFFLINE_MS = 10000;

/**
 * Applies a patch to one node anywhere in the tree, cloning only along the path
 * to it so untouched subtrees keep their identity (and their memoized index).
 * Ids are unique across the tree, so the first hit wins.
 */
function patchTree(nodes: Feature[], id: number, patch: Partial<Feature>): Feature[] {
  let changed = false;
  const next = nodes.map((node) => {
    if (node.id === id) {
      changed = true;
      const merged: Feature = { ...node, ...patch };
      // A server echo describes one record and omits `children` — keep ours.
      if (!merged.children && node.children) merged.children = node.children;
      return merged;
    }
    if (node.children?.length) {
      const children = patchTree(node.children, id, patch);
      if (children !== node.children) {
        changed = true;
        return { ...node, children };
      }
    }
    return node;
  });
  return changed ? next : nodes;
}

export interface TrainerState {
  features: Feature[];
  process: ProcessBlock | null;
  trainer: TrainerBlock | null;
  online: boolean;
  loading: boolean;
  error: string | null;
  /**
   * What the trainer said about the last attach: `attached`, or `waiting` when
   * it took the name but the process isn't running yet (it will bind itself
   * when the game appears). Null once nothing is pending.
   */
  attachStatus: string | null;
  /**
   * The trainer is mid-injection and its state is locked. Distinct from
   * `!online`: it is answering, just with nothing to say yet.
   */
  busy: boolean;
  refresh: () => Promise<void>;
  toggle: (feature: Feature, active: boolean) => Promise<void>;
  setValue: (feature: Feature, value: string) => Promise<void>;
  attach: (processName: string) => Promise<void>;
}

export function useTrainer(enabled: boolean = true): TrainerState {
  const [features, setFeatures] = useState<Feature[]>([]);
  const [process, setProcess] = useState<ProcessBlock | null>(null);
  const [trainer, setTrainer] = useState<TrainerBlock | null>(null);
  const [online, setOnline] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [attachStatus, setAttachStatus] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const mounted = useRef(true);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const inFlight = useRef(false);
  const onlineRef = useRef(false); // read by the poll loop when re-arming
  /**
   * True while an action the trainer is still working on is outstanding.
   *
   * Activating an Auto Assembler script AOB-scans the game's whole address
   * space — 82 seconds, measured, for one script against RDR2. The trainer is
   * busy for all of it, so the 2s poll gets no answer, and without this the
   * panel decides it is offline and offers "Back to launcher" while the
   * activation is in fact still running. Being told a working trainer is dead
   * is what makes someone kill it, and killing it mid-injection is what the
   * game pays for.
   */
  const mutating = useRef(false);

  const applyPayload = useCallback((payload: FeaturesPayload) => {
    setFeatures(payload.features ?? []);
    setProcess(payload.process ?? null);
    setTrainer(payload.trainer ?? null);
    onlineRef.current = true;
    setOnline(true);
    setError(null);
  }, []);

  const reconcile = useCallback((updated: Feature | undefined) => {
    if (!updated || typeof updated.id !== "number") return;
    setFeatures((current) => patchTree(current, updated.id, updated));
  }, []);

  const poll = useCallback(async () => {
    if (inFlight.current || mutating.current) return;
    inFlight.current = true;
    try {
      const payload = await getFeatures();
      if (!mounted.current) return;
      if (payload?.busy) {
        // Injecting. The trainer answered, so it is emphatically not offline —
        // but it sent no tree, and overwriting ours with an empty one would
        // blank the panel mid-activation. Hold what we have.
        onlineRef.current = true;
        setOnline(true);
        setBusy(true);
        setError(null);
        return;
      }
      setBusy(false);
      applyPayload(payload);
    } catch (e) {
      if (!mounted.current) return;
      onlineRef.current = false;
      setOnline(false);
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      inFlight.current = false;
      if (mounted.current) setLoading(false);
    }
  }, [applyPayload]);

  // Poll loop: re-armed after every response so a slow trainer can't stack requests.
  useEffect(() => {
    mounted.current = true;
    if (!enabled) {
      setLoading(false);
      return () => {
        mounted.current = false;
      };
    }

    let cancelled = false;
    const tick = async () => {
      await poll();
      if (cancelled || !mounted.current) return;
      timer.current = setTimeout(tick, onlineRef.current ? POLL_ONLINE_MS : POLL_OFFLINE_MS);
    };
    tick();

    return () => {
      cancelled = true;
      mounted.current = false;
      if (timer.current) clearTimeout(timer.current);
    };
  }, [enabled, poll]);

  const toggle = useCallback(
    async (feature: Feature, active: boolean) => {
      const action = feature.actions?.toggle;
      if (!action) return;
      // Optimistic flip; the response is authoritative (AA activation can fail
      // server-side and echo back the state it actually ended up in).
      setFeatures((current) => patchTree(current, feature.id, { active }));
      mutating.current = true;
      try {
        reconcile(await executeAction(action, feature, { active }));
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
        setFeatures((current) => patchTree(current, feature.id, { active: feature.active }));
      } finally {
        mutating.current = false;
      }
    },
    [reconcile],
  );

  const setValue = useCallback(
    async (feature: Feature, value: string) => {
      const action = feature.actions?.setValue;
      if (!action) return;
      mutating.current = true;
      try {
        reconcile(await executeAction(action, feature, { value }));
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        mutating.current = false;
      }
    },
    [reconcile],
  );

  const attach = useCallback(async (processName: string) => {
    setAttachStatus(null);
    mutating.current = true;
    try {
      // The reply is the only place the difference between "bound now" and
      // "registered, process not running yet" exists — `process.attached` reads
      // false either way, which makes a successful attach look like a dead
      // button. Keep it and let the panel say which happened.
      const result = await attachProcess(processName);
      setAttachStatus(typeof result?.status === "string" ? result.status : null);
      // Released before the refresh: `poll` skips itself while a mutation is
      // outstanding, so holding it here would drop the very read that shows
      // the attach worked.
      mutating.current = false;
      await poll();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      mutating.current = false;
    }
  }, [poll]);

  return {
    features,
    process,
    trainer,
    online,
    loading,
    error,
    attachStatus,
    busy,
    refresh: poll,
    toggle,
    setValue,
    attach,
  };
}
