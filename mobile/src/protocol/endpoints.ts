/**
 * Pure endpoint resolution, classification, and merging helpers for
 * LAN -> Tailscale -> Relay automatic fallback.
 */

export type EndpointRouteKind = "lan" | "tailscale" | "relay" | "direct";

export function normalizeDirectWsUrl(raw: string): string {
  const addr = raw.trim();
  if (!addr) return "";
  return addr.startsWith("ws://") || addr.startsWith("wss://")
    ? addr
    : `ws://${addr}`;
}

export function normalizeRelayClientUrl(relayUrl: string, hostId: string): string {
  const base = relayUrl.trim().replace(/\/+$/, "");
  if (!base) return "";
  const prefix = base.endsWith("/v1") ? base : `${base}/v1`;
  return `${prefix}/client/${encodeURIComponent(hostId)}`;
}

export function extractHostFromAddrOrUrl(addrOrUrl: string): string {
  const trimmed = addrOrUrl
    .trim()
    .replace(/^wss?:\/\//i, "")
    .replace(/^https?:\/\//i, "");
  const hostPort = trimmed.split("/")[0] ?? "";
  if (hostPort.startsWith("[")) {
    const end = hostPort.indexOf("]");
    return end > 0 ? hostPort.slice(1, end).toLowerCase() : hostPort.toLowerCase();
  }
  return (hostPort.split(":")[0] ?? "").toLowerCase();
}

export function isLoopbackHost(addrOrUrl: string): boolean {
  const host = extractHostFromAddrOrUrl(addrOrUrl);
  return host === "localhost" || host === "::1" || host.startsWith("127.");
}

export function isTailscaleHost(addrOrUrl: string): boolean {
  const host = extractHostFromAddrOrUrl(addrOrUrl);
  if (host.endsWith(".ts.net")) return true;
  const parts = host.split(".");
  if (parts.length !== 4) return false;
  const a = Number(parts[0]);
  const b = Number(parts[1]);
  const c = Number(parts[2]);
  const d = Number(parts[3]);
  if (
    !Number.isInteger(a) ||
    !Number.isInteger(b) ||
    !Number.isInteger(c) ||
    !Number.isInteger(d)
  ) {
    return false;
  }
  return a === 100 && b >= 64 && b <= 127 && c >= 0 && c <= 255 && d >= 0 && d <= 255;
}

export function isPrivateLanHost(addrOrUrl: string): boolean {
  const host = extractHostFromAddrOrUrl(addrOrUrl);
  const parts = host.split(".");
  if (parts.length !== 4) return false;
  const a = Number(parts[0]);
  const b = Number(parts[1]);
  if (!Number.isInteger(a) || !Number.isInteger(b)) return false;
  return (
    (a === 192 && b === 168) ||
    a === 10 ||
    (a === 172 && b >= 16 && b <= 31)
  );
}

export function classifyEndpointRoute(
  url: string,
  relayUrl?: string,
  hostId?: string
): EndpointRouteKind {
  if (relayUrl) {
    const relayBase = relayUrl.trim().replace(/\/+$/, "");
    if (
      (hostId && url === normalizeRelayClientUrl(relayUrl, hostId)) ||
      (relayBase && url.startsWith(relayBase))
    ) {
      return "relay";
    }
  }
  if (isTailscaleHost(url)) return "tailscale";
  if (isPrivateLanHost(url)) return "lan";
  return "direct";
}

export function displayEndpointAddress(url: string): string {
  return url.trim().replace(/^wss?:\/\//i, "").replace(/\/v1\/client\/.*$/, "");
}

/**
 * Deduplicates and normalizes a list of direct host:port / ws:// endpoints.
 */
export function normalizeDirectAddrList(
  entries: Array<string | null | undefined>
): string[] {
  const out: string[] = [];
  for (const raw of entries) {
    if (!raw) continue;
    for (const part of raw.split(",")) {
      const trimmed = part.trim();
      if (trimmed && !out.includes(trimmed)) {
        out.push(trimmed);
      }
    }
  }
  return out;
}

/**
 * Builds the ordered, deduplicated WebSocket endpoint list to try:
 * primary & fallback direct addresses (LAN -> Tailscale) followed by relay.
 */
export function buildEndpointUrls(input: {
  hostId: string;
  directAddr?: string;
  directAddrs?: string[];
  relayUrl?: string;
}): string[] {
  const urls: string[] = [];
  const pushUnique = (url: string) => {
    if (url && !urls.includes(url)) {
      urls.push(url);
    }
  };

  const allDirect = normalizeDirectAddrList([
    input.directAddr,
    ...(input.directAddrs ?? []),
  ]);
  for (const addr of allDirect) {
    pushUnique(normalizeDirectWsUrl(addr));
  }

  if (input.relayUrl) {
    pushUnique(normalizeRelayClientUrl(input.relayUrl, input.hostId));
  }

  return urls;
}

/**
 * Merges host-reported direct addresses into saved direct endpoints:
 * prefers the host's fresh list (so DHCP LAN IP changes replace stale IPs),
 * while preserving any existing Tailscale fallback if not already present.
 */
export function mergeAdvertisedDirectAddrs(
  existing: Array<string | null | undefined>,
  advertised: Array<string | null | undefined>
): string[] {
  const fresh = normalizeDirectAddrList(advertised);
  const current = normalizeDirectAddrList(existing);
  if (fresh.length === 0) return current;

  const out = [...fresh];
  for (const addr of current) {
    if (isTailscaleHost(addr) && !out.includes(addr)) {
      out.push(addr);
    }
  }
  return out;
}
