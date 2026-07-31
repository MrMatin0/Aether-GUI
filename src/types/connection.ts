// Mirrors src-tauri/src/state.rs::ConnectionState (serde internally-tagged
// via `#[serde(tag = "state")]`) and src-tauri/src/aether/profiles.rs.

export type ConnectionStatus =
  | { state: "Idle" }
  | { state: "Launching" }
  | { state: "Connecting" }
  | { state: "Connected"; socks_addr: string; connected_at_ms: number }
  | { state: "Reconnecting"; attempt: number; max_attempts: number }
  | { state: "Disconnecting" }
  | { state: "Error"; message: string; phase: string };

export type ConnectionStateName = ConnectionStatus["state"];

/**
 * Mirrors src-tauri/src/error.rs::AetherError's custom Serialize impl.
 *
 * The backend deliberately ships a stable machine-readable `code` so the UI
 * never has to substring-match a human-readable message. The old frontend
 * threw that away with `String(e)` — which on an object rejection yields
 * "[object Object]" — so the sidecar-error routing could never fire and every
 * failure rendered as literal "[object Object]" in the status line.
 */
export type AetherErrorCode =
  | "already_running"
  | "binary_missing"
  | "spawn_failed"
  | "port_in_use"
  | "not_connected"
  | "internal";

export interface AetherErrorPayload {
  code: AetherErrorCode;
  message: string;
}

export function isAetherError(value: unknown): value is AetherErrorPayload {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as AetherErrorPayload).code === "string" &&
    typeof (value as AetherErrorPayload).message === "string"
  );
}

/** Normalises anything `invoke()` can reject with into a typed error. */
export function toAetherError(value: unknown): AetherErrorPayload {
  if (isAetherError(value)) return value;
  if (value instanceof Error) return { code: "internal", message: value.message };
  return { code: "internal", message: String(value) };
}

export type Protocol = "auto" | "masque" | "wireguard" | "gool";
export type ScanMode = "turbo" | "balanced" | "thorough" | "stealth" | "ironclad";
export type IpVersion = "v4" | "v6" | "both";
export type MasqueNoize = "firewall" | "gfw" | "off";
export type WgNoize = "balanced" | "aggressive" | "light" | "off";
export type ZeroTrustAuth = "email" | "service" | "token";

export interface ConnectionProfile {
  protocol: Protocol;
  scan_mode: ScanMode;
  ip_version: IpVersion;
  /** Aether >= 1.1.1: reuse the last known-working gateway with a quick
   * recheck instead of a full scan. */
  quick_reconnect: boolean;
  /** Aether >= 1.2.0: run MASQUE over HTTP/2 (TCP) instead of the default
   * HTTP/3 (QUIC) - for networks that block or throttle UDP. */
  masque_http2: boolean;
  /** Obfuscation profile for MASQUE (firewall/gfw/off). */
  masque_noize: MasqueNoize;
  /** Obfuscation profile for WireGuard/gool (balanced/aggressive/light/off). */
  wg_noize: WgNoize;
  /** Local SOCKS5 listen address (--bind). Default 127.0.0.1:1819. */
  bind_address: string;
  /** Aether >= 1.5.0: optional comma-separated DNS resolvers inside the tunnel. */
  dns: string;
  /** Cloudflare Zero Trust team. Empty keeps the normal consumer WARP flow. */
  zero_trust_team: string;
  zero_trust_auth: ZeroTrustAuth;
  /** Zero Trust credentials stay in memory only; they are never persisted. */
  access_email: string;
  access_client_id: string;
  access_client_secret: string;
  access_token: string;
  /** Route HTTP/HTTPS through the organization's Gateway proxy. */
  zero_trust_gateway: boolean;
  /** Aether >= 1.5.0 traffic-routing rules. */
  route_block: string;
  route_direct: string;
  routes_file: string;
}

/** Wire shape of a single line inside an `aether://log` batch. */
export interface LogEventPayload {
  line: string;
  timestamp: number;
}

/** `aether://log` now carries a coalesced batch rather than one line. */
export interface LogBatchPayload {
  lines: LogEventPayload[];
}

/** `aether://access-code` - a first-class prompt signal, not a log grep. */
export interface AccessCodePayload {
  sequence: number;
  requested_at_ms: number;
}

/**
 * Stored log line. `id` is a client-side monotonic key: React lists keyed by
 * array index re-key every single row whenever the 500-entry ring rolls over,
 * forcing a full re-reconciliation of the log panel ~10x/second during a scan.
 */
export interface LogLine extends LogEventPayload {
  id: number;
}
