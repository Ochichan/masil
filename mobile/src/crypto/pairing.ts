import { fromByteArray, toByteArray } from "base64-js";
import { KEY_LEN } from "./noise-nk";

export interface PairingOffer {
  version: number;
  hostId: string;
  hostPub: Uint8Array;
  pairToken: string;
  relayUrl?: string;
  directAddr?: string;
  hostName: string;
}

export function base64UrlToBytes(input: string): Uint8Array {
  const padded =
    input.replace(/-/g, "+").replace(/_/g, "/") +
    "=".repeat((4 - (input.length % 4)) % 4);
  return toByteArray(padded);
}

export function bytesToBase64(bytes: Uint8Array): string {
  return fromByteArray(bytes);
}

export function base64ToBytes(b64: string): Uint8Array {
  return toByteArray(b64);
}

export function parsePairingUri(uri: string): PairingOffer {
  const trimmed = uri.trim();
  if (!trimmed.startsWith("masil://pair?")) {
    throw new Error("Pairing URI must start with masil://pair?");
  }
  const query = trimmed.slice("masil://pair?".length);
  const params = new Map<string, string>();
  for (const part of query.split("&")) {
    if (!part) continue;
    const eq = part.indexOf("=");
    if (eq <= 0) continue;
    const k = part.slice(0, eq);
    const v = decodeURIComponent(part.slice(eq + 1));
    params.set(k, v);
  }
  const vRaw = params.get("v");
  if (vRaw !== "1") {
    throw new Error(`Unsupported pairing version: ${vRaw ?? "missing"}`);
  }
  const hostId = params.get("host");
  const pubRaw = params.get("pub");
  const pairToken = params.get("pair");
  if (!hostId || !pubRaw || !pairToken) {
    throw new Error("Pairing URI is missing required fields (host, pub, pair)");
  }
  const hostPub = base64UrlToBytes(pubRaw);
  if (hostPub.length !== KEY_LEN) {
    throw new Error("Host public key must be 32 bytes");
  }
  const relayUrl = params.get("relay") || undefined;
  const directAddr = params.get("direct") || undefined;
  if (!relayUrl && !directAddr) {
    throw new Error("Pairing URI must specify relay or direct address");
  }
  return {
    version: 1,
    hostId,
    hostPub,
    pairToken,
    relayUrl,
    directAddr,
    hostName: params.get("name") || hostId,
  };
}
