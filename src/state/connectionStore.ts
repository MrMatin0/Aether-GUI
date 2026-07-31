import { create } from "zustand";
import { useShallow } from "zustand/react/shallow";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  toAetherError,
  type AccessCodePayload,
  type ConnectionProfile,
  type ConnectionStatus,
  type LogBatchPayload,
  type LogLine,
  type MasqueNoize,
  type WgNoize,
  type ZeroTrustAuth,
} from "@/types/connection";

const MAX_LOG_LINES = 500;

interface ConnectionState {
  status: ConnectionStatus;
  profile: ConnectionProfile;
  logs: LogLine[];
  sidecarError: string | null;
  /** Aether's own route-probe budget in seconds, parsed live out of its log
   * stream. Reset on every fresh attempt. */
  scanBudgetSecs: number | null;
  /** Monotonic key for controls that must reset between connection attempts.
   * Bumped by a user-initiated connect AND by every backend-driven
   * `Launching` (i.e. an auto-retry), because attempt-scoped component state
   * is just as stale after a reconnect as after a manual reconnect. */
  attemptId: number;
  /** Latest `sequence` the backend has asked a Zero Trust code for. */
  accessCodeRequested: number;
  /** Highest `sequence` the user has already answered. */
  accessCodeAnswered: number;
  connect: () => Promise<void>;
  disconnect: () => Promise<void>;
  submitAccessCode: (code: string) => Promise<void>;
  setProtocol: (protocol: ConnectionProfile["protocol"]) => void;
  setScanMode: (scan_mode: ConnectionProfile["scan_mode"]) => void;
  setIpVersion: (ip_version: ConnectionProfile["ip_version"]) => void;
  setQuickReconnect: (quick_reconnect: boolean) => void;
  setMasqueHttp2: (masque_http2: boolean) => void;
  setMasqueNoize: (masque_noize: MasqueNoize) => void;
  setWgNoize: (wg_noize: WgNoize) => void;
  setBindAddress: (bind_address: string) => void;
  setDns: (dns: string) => void;
  setZeroTrustTeam: (zero_trust_team: string) => void;
  setZeroTrustAuth: (zero_trust_auth: ZeroTrustAuth) => void;
  setAccessEmail: (access_email: string) => void;
  setAccessClientId: (access_client_id: string) => void;
  setAccessClientSecret: (access_client_secret: string) => void;
  setAccessToken: (access_token: string) => void;
  setZeroTrustGateway: (zero_trust_gateway: boolean) => void;
  setRouteBlock: (route_block: string) => void;
  setRouteDirect: (route_direct: string) => void;
  setRoutesFile: (routes_file: string) => void;
  retryAfterSidecarError: () => void;
}

export const DEFAULT_PROFILE: ConnectionProfile = {
  protocol: "auto",
  scan_mode: "balanced",
  ip_version: "v4",
  quick_reconnect: true,
  masque_http2: false,
  masque_noize: "firewall",
  wg_noize: "balanced",
  bind_address: "127.0.0.1:1819",
  dns: "",
  zero_trust_team: "",
  zero_trust_auth: "email",
  access_email: "",
  access_client_id: "",
  access_client_secret: "",
  access_token: "",
  zero_trust_gateway: false,
  route_block: "",
  route_direct: "",
  routes_file: "",
};

/** Monotonic id source for log rows; never resets, so React keys stay stable
 * across the ring buffer rolling over. */
let nextLogId = 0;

/** Shared patch applied whenever a brand-new attempt begins, from either side
 * of the IPC boundary. */
function freshAttemptPatch(s: ConnectionState) {
  return {
    logs: [] as LogLine[],
    scanBudgetSecs: null,
    attemptId: s.attemptId + 1,
    accessCodeRequested: 0,
    accessCodeAnswered: 0,
  };
}

export const useConnectionStore = create<ConnectionState>()((set, get) => ({
  status: { state: "Idle" },
  profile: DEFAULT_PROFILE,
  logs: [],
  sidecarError: null,
  scanBudgetSecs: null,
  attemptId: 0,
  accessCodeRequested: 0,
  accessCodeAnswered: 0,

  connect: async () => {
    set(freshAttemptPatch(get()));
    try {
      await invoke("connect", { profileOverride: get().profile });
    } catch (e) {
      // The backend serialises AetherError as { code, message }. Branch on the
      // stable discriminant, never on the prose.
      const err = toAetherError(e);
      if (err.code === "binary_missing" || err.code === "spawn_failed") {
        set({ sidecarError: err.message });
      } else {
        set({ status: { state: "Error", message: err.message, phase: "launching" } });
      }
    }
  },

  disconnect: async () => {
    try {
      await invoke("disconnect");
    } catch {
      // Backend rejects disconnect() when there is nothing to stop; status
      // already reflects that, so there is nothing for the UI to do.
    }
  },

  submitAccessCode: async (code) => {
    const pending = get().accessCodeRequested;
    await invoke("submit_access_code", { code });
    set({ accessCodeAnswered: pending });
  },

  setProtocol: (protocol) => set((s) => ({ profile: { ...s.profile, protocol } })),
  setScanMode: (scan_mode) => set((s) => ({ profile: { ...s.profile, scan_mode } })),
  setIpVersion: (ip_version) => set((s) => ({ profile: { ...s.profile, ip_version } })),
  setQuickReconnect: (quick_reconnect) =>
    set((s) => ({ profile: { ...s.profile, quick_reconnect } })),
  setMasqueHttp2: (masque_http2) => set((s) => ({ profile: { ...s.profile, masque_http2 } })),
  setMasqueNoize: (masque_noize) => set((s) => ({ profile: { ...s.profile, masque_noize } })),
  setWgNoize: (wg_noize) => set((s) => ({ profile: { ...s.profile, wg_noize } })),
  setBindAddress: (bind_address) => set((s) => ({ profile: { ...s.profile, bind_address } })),
  setDns: (dns) => set((s) => ({ profile: { ...s.profile, dns } })),
  setZeroTrustTeam: (zero_trust_team) =>
    set((s) => ({ profile: { ...s.profile, zero_trust_team } })),

  setZeroTrustAuth: (zero_trust_auth) =>
    set((s) => ({
      profile: {
        ...s.profile,
        zero_trust_auth,
        access_email: "",
        access_client_id: "",
        access_client_secret: "",
        access_token: "",
      },
    })),

  setAccessEmail: (access_email) => set((s) => ({ profile: { ...s.profile, access_email } })),
  setAccessClientId: (access_client_id) =>
    set((s) => ({ profile: { ...s.profile, access_client_id } })),
  setAccessClientSecret: (access_client_secret) =>
    set((s) => ({ profile: { ...s.profile, access_client_secret } })),
  setAccessToken: (access_token) => set((s) => ({ profile: { ...s.profile, access_token } })),
  setZeroTrustGateway: (zero_trust_gateway) =>
    set((s) => ({ profile: { ...s.profile, zero_trust_gateway } })),
  setRouteBlock: (route_block) => set((s) => ({ profile: { ...s.profile, route_block } })),
  setRouteDirect: (route_direct) => set((s) => ({ profile: { ...s.profile, route_direct } })),
  setRoutesFile: (routes_file) => set((s) => ({ profile: { ...s.profile, routes_file } })),

  retryAfterSidecarError: () => set({ sidecarError: null }),
}));

/**
 * Selector for the Zero Trust code prompt. Previously the component derived
 * this by counting a marker string inside the rolling `logs` array - which
 * meant (a) an O(500) scan on every 100ms flush, and (b) the prompt silently
 * vanishing as soon as the ring buffer evicted the marker mid-scan.
 */
export function useAccessCodePending(): boolean {
  return useConnectionStore(
    (s) => s.accessCodeRequested > 0 && s.accessCodeRequested > s.accessCodeAnswered,
  );
}

/** Stable multi-field selector helper for components that need several
 * profile fields without re-rendering on every unrelated store write. */
export function useProfileFields<T extends keyof ConnectionProfile>(
  ...keys: T[]
): Pick<ConnectionProfile, T> {
  return useConnectionStore(
    useShallow((s) => {
      const out = {} as Pick<ConnectionProfile, T>;
      for (const k of keys) out[k] = s.profile[k];
      return out;
    }),
  );
}

if (import.meta.env.DEV) {
  (window as unknown as { __conn?: typeof useConnectionStore }).__conn = useConnectionStore;
}

const BUDGET_RE = /budget=(\d+)s/;

/** Guards against React 19 StrictMode's double-invoked effects registering
 * two independent listener sets (which duplicated every log line in dev). */
let activeListeners: Promise<() => void> | null = null;

/** Call once from App's top-level effect; returns a cleanup function. */
export function initConnectionListeners(): Promise<() => void> {
  activeListeners ??= startConnectionListeners();
  const handle = activeListeners;
  return handle.then((stop) => () => {
    if (activeListeners === handle) activeListeners = null;
    stop();
  });
}

async function startConnectionListeners(): Promise<() => void> {
  // Log lines arrive fast during route scanning. The backend already batches
  // on a ~120ms cadence; this second window absorbs bursts of batches and
  // keeps the store to one write per frame-ish.
  let pending: LogLine[] = [];
  let flushTimer: ReturnType<typeof setTimeout> | null = null;

  const flushLogs = () => {
    flushTimer = null;
    if (pending.length === 0) return;
    const batch = pending;
    pending = [];
    let budget: number | null = null;
    for (const l of batch) {
      const m = BUDGET_RE.exec(l.line);
      if (m) budget = Number(m[1]);
    }
    useConnectionStore.setState((s) => {
      // Trim in place rather than allocating a full copy of both arrays: the
      // old `[...s.logs, ...batch].slice(-500)` allocated two arrays per
      // flush, ten times a second, for the whole duration of a scan.
      const next = s.logs.length + batch.length > MAX_LOG_LINES
        ? s.logs.slice(s.logs.length + batch.length - MAX_LOG_LINES).concat(batch)
        : s.logs.concat(batch);
      return budget !== null ? { logs: next, scanBudgetSecs: budget } : { logs: next };
    });
  };

  const [unlistenStatus, unlistenLog, unlistenCode] = await Promise.all([
    listen<ConnectionStatus>("aether://status", (e) => {
      const status = e.payload;
      useConnectionStore.setState((s) => {
        if (status.state !== "Launching") return { status };
        // Every Launching - including a backend-driven auto-retry - starts a
        // genuinely fresh attempt. Attempt-scoped component state keys off
        // attemptId, so it must advance here too, not only in connect().
        return { status, ...freshAttemptPatch(s) };
      });
    }),
    listen<LogBatchPayload>("aether://log", (e) => {
      for (const line of e.payload.lines) {
        pending.push({ ...line, id: nextLogId++ });
      }
      flushTimer ??= setTimeout(flushLogs, 100);
    }),
    listen<AccessCodePayload>("aether://access-code", (e) => {
      useConnectionStore.setState({ accessCodeRequested: e.payload.sequence });
    }),
  ]);

  // Reconcile state in case the window reopened mid-session, and load the
  // last-successful profile. Never clobber a profile the user has already
  // started editing while this was in flight.
  try {
    const [status, profile] = await Promise.all([
      invoke<ConnectionStatus>("get_status"),
      invoke<ConnectionProfile>("get_default_profile"),
    ]);
    useConnectionStore.setState((s) => ({
      status,
      profile: s.profile === DEFAULT_PROFILE ? profile : s.profile,
    }));
  } catch (e) {
    console.error("Failed to load initial connection state:", toAetherError(e).message);
  }

  return () => {
    unlistenStatus();
    unlistenLog();
    unlistenCode();
    if (flushTimer !== null) clearTimeout(flushTimer);
    pending = [];
  };
}
